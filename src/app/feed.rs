//! The首页 feed: cards, covers, prefetch ring, warm-up and history/稍后再看 lists.

use crate::*;

/// Cover fetches in flight for the preload ring's warm-up. Matches
/// [`THUMB_CONCURRENCY`]: a page of covers then lands in ~2 s on a typical CDN,
/// inside [`COVER_WARM_BUDGET`], instead of trickling past it.
pub(crate) const COVER_WARM_IN_FLIGHT: usize = 6;

/// Covers warmed per ring batch: one client page (see
/// [`feed_cover_warm_target`]). Never more than a 24-card page, which also
/// bounds what the ring can pin in memory.
pub(crate) const COVER_WARM_MAX: usize = 24;

/// A small margin past the page, so the first wheel nudge already has covers.
pub(crate) const COVER_WARM_MARGIN: usize = 4;

/// Wall-clock budget for one batch's warm-up. A slow CDN must not push the
/// startup wait past [`FEED_HOME_WAIT`]: whatever landed by the deadline is
/// attached, the rest streams after promotion exactly as before.
pub(crate) const COVER_WARM_BUDGET: Duration = Duration::from_millis(2500);

/// Bound for the decoded-cover cache both the streaming decoder and the
/// warm-up read: entries and pixel bytes. Eviction is least-recently-used.
pub(crate) const COVER_CACHE_ENTRIES: usize = 128;

pub(crate) const COVER_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Feed source of the preload ring: 0 = 首页推荐. The ring is tagged so a second
/// source can be added later without touching the promotion path.
pub(crate) const FEED_SOURCE_RECOMMEND: i32 = 0;

/// A ready batch older than this is dropped (bounded memory) and refetched.
pub(crate) const FEED_PREFETCH_TTL: Duration = Duration::from_secs(10 * 60);

/// How often the background filler looks for work (and for a session).
pub(crate) const FEED_PREFETCH_POLL: Duration = Duration::from_millis(250);

/// The startup path waits this long for the ring's first batch before showing
/// the home view anyway.
pub(crate) const FEED_HOME_WAIT: Duration = Duration::from_secs(6);

/// The `fresh_idx` cursor the startup/refresh load has always used.
pub(crate) const FEED_START_CURSOR: u64 = 1;

/// 播放闸门：任何播放器开着就等，和养鱼 ASR 一样把“暂停 / 继续”写进日志。
///
/// 返回等待的毫秒数（0 = 没等）。
pub(crate) async fn warm_gate(tab: WarmTab) -> u64 {
    let started = Instant::now();
    let mut logged = false;
    while player_active() {
        if !logged {
            logged = true;
            crate::debug_log(&format!("warm paused tab={} reason=playback", tab.label()));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if logged {
        let waited = started.elapsed().as_millis() as u64;
        crate::debug_log(&format!(
            "warm resumed tab={} waited_ms={waited}",
            tab.label()
        ));
        return waited;
    }
    0
}

/// 队列闸门：一个 tab 开始之前先等播放器全部关掉。
///
/// 这个等待**不计入** [`WARM_TASK_LIMIT`]——播放中的 30 秒不该把一个 tab 判成
/// 超时（第一版就是这样，8 个 tab 全部 `ok=0`）。
pub(crate) async fn warm_wait_idle(tab: WarmTab) {
    let started = Instant::now();
    let mut logged = false;
    while player_active() {
        if !logged {
            logged = true;
            crate::debug_log(&format!("warm paused tab={} reason=playback", tab.label()));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if logged {
        crate::debug_log(&format!(
            "warm resumed tab={} waited_ms={}",
            tab.label(),
            started.elapsed().as_millis()
        ));
    }
}

/// 一个 tab 的 15 秒预算：只在“没有播放器”的时间里计时。
///
/// 用 100 ms 的轮询包住任务本身，而不是 `timeout(15s, …)`：播放中途开始时任务
/// 会停在 [`warm_gate`] 里，暂停的那段时间不消费预算，播完接着跑完同一个 tab。
pub(crate) async fn warm_budgeted(
    session: &Arc<bilibili::Session>,
    tab: WarmTab,
    seen: &Arc<Mutex<HashSet<String>>>,
) -> Option<(usize, usize, usize, u64)> {
    let mut work = Box::pin(warm_tab(session, tab, seen));
    let mut active = Duration::ZERO;
    let mut last = Instant::now();
    loop {
        match tokio::time::timeout(WARM_TICK, work.as_mut()).await {
            Ok(done) => return Some(done),
            Err(_) => {
                if !player_active() {
                    active = active.saturating_add(last.elapsed());
                }
                last = Instant::now();
                if active >= WARM_TASK_LIMIT {
                    crate::debug_log(&format!(
                        "warm tab={} timeout active_ms={} limit_ms={}",
                        tab.label(),
                        active.as_millis(),
                        WARM_TASK_LIMIT.as_millis()
                    ));
                    return None;
                }
            }
        }
    }
}

pub(crate) fn home_bar_rect(ui: &AppWindow) -> (f32, f32, f32, f32) {
    let size = ui.window().size();
    let w = ui.get_home_bar_w();
    let x = (size.width as f32 - w) / 2.0;
    let y = size.height as f32 - 56.0 - 28.0;
    (x, y, w, 56.0)
}

/// `login-step id=done` 之后等这么久再开始：首页刚显示的两秒留给首帧渲染。
pub(crate) const WARM_START_DELAY: Duration = Duration::from_secs(2);

/// 同时热的 tab 数。两个：够快，又不会和页面的正常请求抢满连接。
pub(crate) const WARM_CONCURRENCY: usize = 2;

/// 单个 tab 的硬上限；超时记 `ok=0` 继续下一个，绝不让一个慢接口堵住队列。
pub(crate) const WARM_TASK_LIMIT: Duration = Duration::from_secs(15);

/// 预算计时的粒度（见 [`warm_budgeted`]）。
pub(crate) const WARM_TICK: Duration = Duration::from_millis(100);

/// 每个 tab 只热首屏这么多张封面。
pub(crate) const WARM_COVERS: usize = 12;

/// 预热取第一页用的分页大小（列表页默认一屏）。
pub(crate) const WARM_PAGE: i64 = 24;

/// 队列顺序（也是 `tabs=8/8` 的分母）。
pub(crate) const WARM_TABS: [WarmTab; 8] = [
    WarmTab::Fav,
    WarmTab::History,
    WarmTab::ToView,
    WarmTab::Region,
    WarmTab::Pgc,
    WarmTab::Live,
    WarmTab::Dynamic,
    WarmTab::Messages,
];

/// 预热阶段：`0` 未启动 / `1` 等 2 秒 / `2` 队列在跑 / `3` 结束。
pub(crate) static WARM_PHASE: AtomicU32 = AtomicU32::new(0);

/// 已完成（含超时、失败）的 tab 数。
pub(crate) static WARM_DONE_TABS: AtomicUsize = AtomicUsize::new(0);

/// 本轮队列的 tab 数（已跳过的不算）。
pub(crate) static WARM_TOTAL_TABS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn warm_enabled() -> bool {
    !matches!(
        std::env::var("BILI_WARM").ok().as_deref().map(str::trim),
        Some("0") | Some("off") | Some("false")
    )
}

/// 登录成功后启动预热队列。
///
/// 调用点必须在 `post_step(id="done")` 之后（真正的“完成”），队列自己再等
/// [`WARM_START_DELAY`]。`generation`/`my` 只用来判断这轮登录有没有被新的
/// 尝试顶掉：顶掉了就什么都不做，绝不把上一个账号的页面热给新账号。
pub(crate) fn start_tab_warm(
    generation: Arc<AtomicU64>,
    my: u64,
    session_slot: SessionSlot,
    settings_store: Arc<Mutex<settings::Settings>>,
) {
    if !warm_enabled() {
        WARM_PHASE.store(3, Ordering::SeqCst);
        crate::debug_log("warm skipped reason=disabled");
        return;
    }
    let tabs: Vec<WarmTab> = match settings_store.lock() {
        Ok(prefs) => WARM_TABS
            .iter()
            .copied()
            .filter(|tab| tab.enabled(&prefs))
            .collect(),
        Err(_) => WARM_TABS.to_vec(),
    };
    if tabs.is_empty() {
        WARM_PHASE.store(3, Ordering::SeqCst);
        crate::debug_log("warm skipped reason=no-tabs");
        return;
    }
    WARM_TOTAL_TABS.store(tabs.len(), Ordering::SeqCst);
    WARM_DONE_TABS.store(0, Ordering::SeqCst);
    WARM_PHASE.store(1, Ordering::SeqCst);
    let spawned = std::thread::Builder::new()
        .name("tab-warm".to_string())
        .spawn(move || {
            crate::debug_log(&format!(
                "warm start tabs={} delay_ms={} concurrency={WARM_CONCURRENCY} covers={WARM_COVERS} limit_ms={}",
                tabs.len(),
                WARM_START_DELAY.as_millis(),
                WARM_TASK_LIMIT.as_millis()
            ));
            std::thread::sleep(WARM_START_DELAY);
            if generation.load(Ordering::SeqCst) != my {
                WARM_PHASE.store(3, Ordering::SeqCst);
                crate::debug_log("warm skipped reason=superseded");
                return;
            }
            let Some(session) = active_session(&session_slot) else {
                WARM_PHASE.store(3, Ordering::SeqCst);
                crate::debug_log("warm skipped reason=no-session");
                return;
            };
            let Some(runtime) = bilibili::shared_runtime() else {
                WARM_PHASE.store(3, Ordering::SeqCst);
                crate::debug_log("warm skipped reason=no-runtime");
                return;
            };
            WARM_PHASE.store(2, Ordering::SeqCst);
            runtime.block_on(warm_queue(session, tabs, generation, my));
            WARM_PHASE.store(3, Ordering::SeqCst);
        });
    if spawned.is_err() {
        WARM_PHASE.store(3, Ordering::SeqCst);
        crate::debug_log("warm skipped reason=thread");
    }
}

/// 2 路（[`WARM_CONCURRENCY`]）跑完整个队列，每个任务单独限时。
pub(crate) async fn warm_queue(
    session: Arc<bilibili::Session>,
    tabs: Vec<WarmTab>,
    generation: Arc<AtomicU64>,
    my: u64,
) {
    let started = Instant::now();
    let total = tabs.len();
    // 队列内跨 tab 的封面去重：同一个视频出现在两个 tab 时只拉一次。
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let tasks = tabs.into_iter().map(|tab| {
        let session = session.clone();
        let seen = seen.clone();
        let generation = generation.clone();
        async move {
            // 一个新的登录尝试会顶掉这轮：剩下的 tab 一律不热，日志留痕。
            if generation.load(Ordering::SeqCst) != my {
                WARM_DONE_TABS.fetch_add(1, Ordering::SeqCst);
                crate::debug_log(&format!(
                    "warm tab={} skipped reason=superseded",
                    tab.label()
                ));
                return WarmReport {
                    tab,
                    ms: 0,
                    items: 0,
                    covers: 0,
                    cached: 0,
                    bytes: 0,
                    ok: false,
                };
            }
            // 队列闸门在预算之外：播放器开着就一直等，不烧掉这个 tab 的 15 秒。
            warm_wait_idle(tab).await;
            let at = Instant::now();
            let outcome = warm_budgeted(&session, tab, &seen).await;
            let ms = at.elapsed().as_millis();
            let report = match outcome {
                Some(warm) => WarmReport {
                    tab,
                    ms,
                    items: warm.0,
                    covers: warm.1,
                    cached: warm.2,
                    bytes: warm.3,
                    ok: true,
                },
                None => WarmReport {
                    tab,
                    ms,
                    items: 0,
                    covers: 0,
                    cached: 0,
                    bytes: 0,
                    ok: false,
                },
            };
            crate::debug_log(&format!(
                "warm tab={} ms={} items={} covers={} cached={} bytes={} ok={}",
                report.tab.label(),
                report.ms,
                report.items,
                report.covers,
                report.cached,
                report.bytes,
                u8::from(report.ok)
            ));
            WARM_DONE_TABS.fetch_add(1, Ordering::SeqCst);
            report
        }
    });
    let reports: Vec<WarmReport> = futures::stream::iter(tasks)
        .buffer_unordered(WARM_CONCURRENCY)
        .collect()
        .await;
    let done = reports.iter().filter(|report| report.ok).count();
    let bytes: u64 = reports.iter().map(|report| report.bytes).sum();
    let covers: usize = reports.iter().map(|report| report.covers).sum();
    let cached: usize = reports.iter().map(|report| report.cached).sum();
    crate::debug_log(&format!(
        "warm done tabs={done}/{total} ms={} bytes={bytes} covers={covers} cached={cached}",
        started.elapsed().as_millis()
    ));
}

/// 一个 tab：第一页 + 首屏封面。
///
/// 返回 `(items, covers, cached, bytes)`。任何一步失败只记日志并返回已经拿到的
/// 部分——预热是尽力而为，永远不重试、不阻塞队列。
pub(crate) async fn warm_tab(
    session: &Arc<bilibili::Session>,
    tab: WarmTab,
    seen: &Arc<Mutex<HashSet<String>>>,
) -> (usize, usize, usize, u64) {
    warm_gate(tab).await;
    let (items, pics) = warm_page(session, tab).await;
    let (covers, cached, bytes) = warm_covers(session, tab, &pics, seen).await;
    (items, covers, cached, bytes)
}

/// 一个 tab 的第一页：返回 `(行数, 首屏封面 URL)`，不碰缓存也不碰 UI。
///
/// 和 [`warm_tab`] 分开是为了 QA：`warm bench` 要单独量“接口这一页”和“封面”
/// 两段耗时，而真正的队列只关心合起来的结果。
pub(crate) async fn warm_page(session: &Arc<bilibili::Session>, tab: WarmTab) -> (usize, Vec<String>) {
    let mut items = 0usize;
    let mut pics: Vec<String> = Vec::new();
    match tab {
        WarmTab::Fav => {
            // 收藏夹 的“全部”和 首个夹子首页 用第一个夹子的第一页；推荐流拿
            // 最大的夹子做种子，所以两页都热（封面在下面统一去重）。
            warm_gate(tab).await;
            let folders = match session.fav_folders().await {
                Ok(folders) => folders,
                Err(err) => {
                    crate::debug_log(&format!("warm tab=fav step=folders err={err:#}"));
                    return (0, Vec::new());
                }
            };
            items += folders.len();
            let first = folders.first().cloned();
            let largest = folders.iter().max_by_key(|folder| folder.count).cloned();
            let mut seeds: Vec<bilibili::FavFolder> = Vec::new();
            for folder in [first, largest].into_iter().flatten() {
                if !seeds.iter().any(|known| known.id == folder.id) {
                    seeds.push(folder);
                }
            }
            for folder in seeds {
                warm_gate(tab).await;
                match session.fav_list(folder.id, 1, WARM_PAGE).await {
                    Ok(page) => {
                        items += page.items.len();
                        pics.extend(page.items.iter().map(|item| item.cover.clone()));
                    }
                    Err(err) => {
                        crate::debug_log(&format!(
                            "warm tab=fav step=list folder={} err={err:#}",
                            folder.id
                        ));
                    }
                }
            }
        }
        WarmTab::History => {
            warm_gate(tab).await;
            match session.history(0, 0, "").await {
                Ok(page) => {
                    items += page.items.len();
                    pics.extend(page.items.iter().map(|item| item.cover.clone()));
                }
                Err(err) => {
                    crate::debug_log(&format!("warm tab=history err={err:#}"));
                }
            }
        }
        WarmTab::ToView => {
            warm_gate(tab).await;
            match session.toview().await {
                Ok(page) => {
                    items += page.items.len();
                    pics.extend(page.items.iter().map(|item| item.cover.clone()));
                }
                Err(err) => {
                    crate::debug_log(&format!("warm tab=toview err={err:#}"));
                }
            }
        }
        WarmTab::Region => {
            // 分区默认页（动画）+ 全站排行第一页。
            let (_, rid) = REGIONS.first().copied().unwrap_or(("动画", 1));
            warm_gate(tab).await;
            match session.region_latest(rid, 1, WARM_PAGE).await {
                Ok(page) => {
                    items += page.items.len();
                    pics.extend(page.items.iter().map(|card| card.pic.clone()));
                }
                Err(err) => {
                    crate::debug_log(&format!("warm tab=region step=latest err={err:#}"));
                }
            }
            warm_gate(tab).await;
            match session.ranking(0).await {
                Ok(cards) => {
                    items += cards.len();
                    pics.extend(cards.iter().take(WARM_PAGE as usize).map(|card| card.pic.clone()));
                }
                Err(err) => {
                    crate::debug_log(&format!("warm tab=region step=rank err={err:#}"));
                }
            }
        }
        WarmTab::Pgc => {
            // 每日放送的“今天” + 番剧索引第一页。
            warm_gate(tab).await;
            match session.pgc_timeline().await {
                Ok(days) => {
                    if let Some(today) = days.iter().find(|day| day.is_today).or(days.first()) {
                        items += today.episodes.len();
                        pics.extend(today.episodes.iter().map(|ep| ep.cover.clone()));
                    }
                }
                Err(err) => {
                    crate::debug_log(&format!("warm tab=pgc step=timeline err={err:#}"));
                }
            }
            warm_gate(tab).await;
            match session
                .pgc_index(1, 1, PGC_PAGE_SIZE, pgc_sort_order(settings::PGC_SORT_DEFAULT))
                .await
            {
                Ok(page) => {
                    items += page.items.len();
                    pics.extend(page.items.iter().map(|item| item.cover.clone()));
                }
                Err(err) => {
                    crate::debug_log(&format!("warm tab=pgc step=index err={err:#}"));
                }
            }
        }
        WarmTab::Live => {
            warm_gate(tab).await;
            match session.live_index().await {
                Ok(index) => {
                    items += index
                        .modules
                        .iter()
                        .map(|module| module.rooms.len())
                        .sum::<usize>();
                    pics.extend(
                        index
                            .modules
                            .iter()
                            .flat_map(|module| module.rooms.iter())
                            .take(WARM_COVERS)
                            .map(|room| room.cover.clone()),
                    );
                }
                Err(err) => {
                    crate::debug_log(&format!("warm tab=live err={err:#}"));
                }
            }
        }
        WarmTab::Dynamic => {
            warm_gate(tab).await;
            match session.dynamic_feed("", "all").await {
                Ok(page) => {
                    items += page.items.len();
                    for item in &page.items {
                        if !item.archive_cover.is_empty() {
                            pics.push(item.archive_cover.clone());
                        }
                        pics.extend(item.images.iter().cloned());
                    }
                }
                Err(err) => {
                    crate::debug_log(&format!("warm tab=dynamic err={err:#}"));
                }
            }
        }
        WarmTab::Messages => {
            // 消息 tab 的首屏是会话列表；第一条会话的首页也跟着热。
            warm_gate(tab).await;
            match session.dm_sessions().await {
                Ok(sessions) => {
                    items += sessions.len();
                    if let Some(first) = sessions.first() {
                        warm_gate(tab).await;
                        match session.dm_messages(first.talker_id).await {
                            Ok(messages) => items += messages.len(),
                            Err(err) => {
                                crate::debug_log(&format!(
                                    "warm tab=messages step=thread err={err:#}"
                                ));
                            }
                        }
                    }
                }
                Err(err) => {
                    crate::debug_log(&format!("warm tab=messages err={err:#}"));
                }
            }
        }
    }
    (items, pics)
}

/// 把这一页的封面拉进 [`cover_cache`]：先看缓存（命中记 `cached`），没命中才
/// 走网络；播放器打开时立刻停手（和列表的增量封面同一个闸门）。
pub(crate) async fn warm_covers(
    session: &Arc<bilibili::Session>,
    tab: WarmTab,
    pics: &[String],
    seen: &Arc<Mutex<HashSet<String>>>,
) -> (usize, usize, u64) {
    let mut wanted: Vec<String> = Vec::new();
    for pic in pics {
        if wanted.len() >= WARM_COVERS {
            break;
        }
        if pic.is_empty() {
            continue;
        }
        // 队列内去重：两个 tab 首屏重合的卡片只算一次。
        let fresh = seen
            .lock()
            .map(|mut seen| seen.insert(pic.clone()))
            .unwrap_or(true);
        if fresh {
            wanted.push(pic.clone());
        }
    }
    if wanted.is_empty() {
        return (0, 0, 0);
    }
    warm_gate(tab).await;
    let fetches = wanted.into_iter().map(|url| {
        let session = session.clone();
        async move {
            if player_active() {
                return (false, false, 0u64);
            }
            let hit = cover_cache()
                .lock()
                .map(|mut cache| cache.get(&url).is_some())
                .unwrap_or(false);
            if hit {
                return (false, true, 0);
            }
            match cover_fetch(&session, &url, CoverPath::Warm).await {
                Some(pixels) => (true, false, pixels.0.len() as u64),
                None => (false, false, 0),
            }
        }
    });
    let mut covers = 0usize;
    let mut cached = 0usize;
    let mut bytes = 0u64;
    let mut stream = futures::stream::iter(fetches).buffer_unordered(THUMB_CONCURRENCY);
    while let Some((fetched, hit, len)) = stream.next().await {
        if hit {
            cached += 1;
        } else if fetched {
            covers += 1;
            bytes = bytes.saturating_add(len);
        }
    }
    (covers, cached, bytes)
}

pub(crate) fn to_thumb(pixels: &Option<(Vec<u8>, u32, u32)>) -> Image {
    match pixels {
        Some((bytes, width, height)) => {
            let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(*width, *height);
            buffer.make_mut_bytes().copy_from_slice(bytes);
            Image::from_rgba8(buffer)
        }
        None => Image::default(),
    }
}

/// [`to_thumb`] for a shared cache entry: the one copy into the Slint buffer is
/// the only copy, so a warm cover is not cloned per patch.
pub(crate) fn to_thumb_shared(pixels: &CoverPixels) -> Image {
    let (bytes, width, height) = pixels;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(*width, *height);
    buffer.make_mut_bytes().copy_from_slice(bytes);
    Image::from_rgba8(buffer)
}

/// Replace one row's cover in an already-rendered `VecModel`, keeping its text.
pub(crate) fn patch_model_thumb<T: Clone + 'static>(
    model: ModelRc<T>,
    index: usize,
    thumb: Image,
    set_thumb: fn(&mut T, Image),
) {
    if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<T>>() {
        if let Some(mut row) = vec_model.row_data(index) {
            set_thumb(&mut row, thumb);
            vec_model.set_row_data(index, row);
        }
    }
}

/// Post one incremental cover patch for a `T`-row list.
pub(crate) fn post_thumb_patch<T: Clone + 'static>(
    weak: &Weak<AppWindow>,
    index: usize,
    pixels: Option<(Vec<u8>, u32, u32)>,
    guard: ThumbGuard,
    get_model: fn(&AppWindow) -> ModelRc<T>,
    set_thumb: fn(&mut T, Image),
    sync: Arc<ThumbSync>,
) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let applied = guard.allows(index);
            if applied {
                patch_model_thumb(get_model(&ui), index, to_thumb(&pixels), set_thumb);
            }
            skeleton_settle(&ui, 1);
            sync.settle(applied);
        }
    });
}

/// Fetches issued by the display path (`stream_thumbs`) and by the ring
/// warm-up, counted separately so the QA hook can prove that a promotion no
/// longer streams the first screenful. Cache hits are not fetches.
pub(crate) static COVER_FETCHES_DISPLAY: AtomicU64 = AtomicU64::new(0);

pub(crate) static COVER_FETCHES_WARM: AtomicU64 = AtomicU64::new(0);

/// Display fetches issued for a row inside the first screenful of a promoted
/// batch. The warm-up exists to make this zero.
pub(crate) static COVER_FETCHES_FIRST_SCREEN: AtomicU64 = AtomicU64::new(0);

pub(crate) fn cover_fetches_display() -> u64 {
    COVER_FETCHES_DISPLAY.load(Ordering::Relaxed)
}

pub(crate) fn cover_fetches_warm() -> u64 {
    COVER_FETCHES_WARM.load(Ordering::Relaxed)
}

pub(crate) fn cover_fetches_first_screen() -> u64 {
    COVER_FETCHES_FIRST_SCREEN.load(Ordering::Relaxed)
}

pub(crate) static COVER_CACHE: OnceLock<Mutex<CoverCache>> = OnceLock::new();

pub(crate) fn cover_cache() -> &'static Mutex<CoverCache> {
    COVER_CACHE.get_or_init(|| Mutex::new(CoverCache::new()))
}

/// QA only: empty the decoded-cover cache so a "cold first screen" can be timed
/// in the same process as the post-warm one. Never compiled into the app.
#[cfg(test)]
pub(crate) fn cover_cache_clear() {
    if let Ok(mut cache) = cover_cache().lock() {
        *cache = CoverCache::new();
    }
}

/// Fetch + decode one cover through the shared cache, skipping the network
/// entirely on a hit and while the player is open. `None` means "skipped or
/// failed"; the list resumes with a fresh fetch the moment the player closes.
/// The lock is never held across the network call.
pub(crate) async fn cover_fetch(
    session: &Arc<bilibili::Session>,
    url: &str,
    path: CoverPath,
) -> Option<Arc<CoverPixels>> {
    if url.is_empty() || player_active() {
        return None;
    }
    if let Ok(mut cache) = cover_cache().lock() {
        if let Some(hit) = cache.get(url) {
            return Some(hit);
        }
    }
    match path {
        CoverPath::Display => &COVER_FETCHES_DISPLAY,
        CoverPath::Warm => &COVER_FETCHES_WARM,
    }
    .fetch_add(1, Ordering::Relaxed);
    // Listed by the stall trace as `covers=`/`last=cover:decode`; the guard
    // also releases the count when the future is cancelled mid-fetch.
    crate::player::tag_here("cover:decode");
    let _decoding = CoverDecodeGuard::new();
    let pixels = Arc::new(session.fetch_thumb(url).await.ok()?);
    if let Ok(mut cache) = cover_cache().lock() {
        cache.put(url, pixels.clone());
    }
    Some(pixels)
}

/// Fetch a small cover on the shared runtime, skipping it entirely while the
/// player is open. `None` means "skipped or failed"; the list resumes with a
/// fresh fetch the moment the player closes.
pub(crate) async fn thumb_gated(session: &Arc<bilibili::Session>, url: &str) -> Option<CoverPixels> {
    cover_fetch(session, url, CoverPath::Display)
        .await
        .map(|pixels| (*pixels).clone())
}

/// Fetch an avatar/cover via `fetch_image`, skipping it while the player is open.
pub(crate) async fn image_gated(session: &Arc<bilibili::Session>, url: &str) -> Option<(Vec<u8>, u32, u32)> {
    if url.is_empty() || player_active() {
        return None;
    }
    session.fetch_image(url).await.ok()
}

/// Stream one list's covers with `THUMB_CONCURRENCY` in flight and hand every
/// arrival to `on_one` (worker thread) the moment it is decoded. `on_issue`
/// runs as each fetch is actually started (the bounded stream only starts the
/// next one when a slot frees), so a caller can count what the list asked the
/// network for. While the player is open no new fetch starts and late arrivals
/// are dropped unpatched.
pub(crate) async fn stream_thumbs<F, I>(
    session: &Arc<bilibili::Session>,
    pics: Vec<String>,
    mut on_issue: I,
    mut on_one: F,
) where
    F: FnMut(usize, Option<(Vec<u8>, u32, u32)>),
    I: FnMut(usize),
{
    let fetches = pics.into_iter().enumerate().map(|(index, pic)| {
        on_issue(index);
        let session = session.clone();
        async move {
            (index, thumb_gated(&session, &pic).await)
        }
    });
    let mut stream = futures::stream::iter(fetches).buffer_unordered(THUMB_CONCURRENCY);
    while let Some((index, pixels)) = stream.next().await {
        if player_active() {
            continue;
        }
        on_one(index, pixels);
    }
}

/// UI thread: arm the skeleton shimmer for `count` pending covers.
pub(crate) fn skeleton_arm(ui: &AppWindow, count: u64) {
    if count == 0 || player_active() {
        return;
    }
    SKELETON.with(|cell| {
        let mut state = cell.borrow_mut();
        state.remaining += count;
        state.armed_at = Instant::now();
        ui.global::<AppTheme>().set_skeletons_on(true);
        if !state.timer.running() {
            let weak = ui.as_weak();
            state.timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(90),
                move || skeleton_tick(&weak),
            );
        }
    });
}

/// UI thread: stop the skeleton shimmer immediately (used when the player opens,
/// so no timer keeps repainting the hidden main window during playback).
pub(crate) fn skeleton_halt(ui: &AppWindow) {
    SKELETON.with(|cell| {
        let mut state = cell.borrow_mut();
        state.remaining = 0;
        state.timer.stop();
    });
    ui.global::<AppTheme>().set_skeletons_on(false);
}

pub(crate) fn skeleton_tick(weak: &Weak<AppWindow>) {
    SKELETON.with(|cell| {
        let mut state = cell.borrow_mut();
        let timed_out = state.armed_at.elapsed() > Duration::from_secs(30);
        if state.remaining == 0 || timed_out {
            state.remaining = 0;
            state.timer.stop();
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppTheme>().set_skeletons_on(false);
            }
            return;
        }
        state.phase = (state.phase + 0.11) % 1.0;
        if let Some(ui) = weak.upgrade() {
            ui.global::<AppTheme>().set_skeleton_phase(state.phase);
        }
    });
}

/// UI thread: `count` pending covers settled (patched, superseded or empty).
pub(crate) fn skeleton_settle(ui: &AppWindow, count: u64) {
    SKELETON.with(|cell| {
        let mut state = cell.borrow_mut();
        state.remaining = state.remaining.saturating_sub(count);
        if state.remaining == 0 && state.timer.running() {
            state.timer.stop();
            ui.global::<AppTheme>().set_skeletons_on(false);
        }
    });
}

/// Build the text-only UI row for a feed video (empty cover; UI thread only).
pub(crate) fn build_feed_row_text(video: &bilibili::FeedVideo) -> FeedRow {
    FeedRow {
        thumb: Image::default(),
        // The queue identity is stable across recycling; `queued` is refreshed
        // by `Playlist::mark_feed_queued` on every queue mutation.
        bvid: video.bvid.clone().into(),
        queued: false,
        title: video.title.clone().into(),
        owner: video.owner.clone().into(),
        owner_mid: video.owner_mid.to_string().into(),
        meta: if video.view == 0 {
            // `newlist` (the region feed's working endpoint) reports
            // `stat.view = 0` for every card; printing "0播放" would be a
            // number the API never actually measured. Show the duration only.
            format_duration(video.duration).into()
        } else {
            format!(
                "{}播放 · {}",
                format_view(video.view),
                format_duration(video.duration)
            )
            .into()
        },
        badge: format_duration(video.duration).into(),
        // The card redesign's own fields: the publish stamp (0 = unknown) and
        // the exact play count, so a card never has to parse `meta`.
        pub_ts: saturating_i32(video.pub_ts),
        view_count: saturating_i32(video.view as i64),
    }
}

/// Covers warmed per ring batch: one client page as laid out at the 480×270
/// minimum window, where a single grid row fits, so a page's cards are what the
/// user sees before scrolling. The margin covers the next row, and
/// [`COVER_WARM_MAX`] caps the work (and the memory the ring can pin).
pub(crate) fn feed_cover_warm_target(page_size: i64) -> usize {
    ((page_size.max(1) as usize).saturating_add(COVER_WARM_MARGIN)).min(COVER_WARM_MAX)
}

/// `BILI_COVER_WARM=0` turns the ring's cover warm-up off. Kept for the
/// before/after QA measurement; the default is on.
pub(crate) fn cover_warm_enabled() -> bool {
    std::env::var("BILI_COVER_WARM")
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
}

/// Decode the first [`feed_cover_warm_target`] covers of a prefetched batch
/// through the same cache and fetch path the visible list uses. Called on the
/// filler thread with no ring lock held; `player_active` skips it like every
/// other cover fetch, and the deadline keeps a slow CDN from delaying the
/// batch (and with it the startup wait). Failures are not fatal: whatever
/// landed is attached and the rest streams after promotion.
pub(crate) fn warm_feed_covers(
    session: &Arc<bilibili::Session>,
    items: &[bilibili::FeedVideo],
    target: usize,
) -> WarmCovers {
    let cap = items.len().min(bilibili::THUMB_MAX_PER_VIEW);
    let mut warm = WarmCovers::new(cap, target);
    let wanted = target.min(cap);
    if wanted == 0 {
        return warm;
    }
    let Some(runtime) = bilibili::shared_runtime() else {
        crate::debug_log(&format!(
            "feed covers warm n={wanted} ok=0 ms=0 reason=runtime"
        ));
        return warm;
    };
    let began = Instant::now();
    let mut rows: Vec<(usize, Arc<CoverPixels>)> = Vec::with_capacity(wanted);
    // The deadline covers the whole warm-up, not just the gaps between
    // arrivals: a stalled request must not hold the batch (and the startup
    // wait) hostage. Dropping the stream cancels the outstanding fetches; their
    // rows simply stream after promotion.
    let truncated = runtime.block_on(async {
        let fetches = items.iter().take(wanted).enumerate().map(|(index, video)| {
            let session = session.clone();
            let pic = video.pic.clone();
            async move { (index, cover_fetch(&session, &pic, CoverPath::Warm).await) }
        });
        let mut stream = futures::stream::iter(fetches).buffer_unordered(COVER_WARM_IN_FLIGHT);
        let pump = async {
            while let Some((index, pixels)) = stream.next().await {
                if let Some(pixels) = pixels {
                    rows.push((index, pixels));
                }
            }
        };
        tokio::time::timeout(COVER_WARM_BUDGET, pump).await.is_err()
    });
    rows.sort_by_key(|(index, _)| *index);
    warm.rows = rows;
    let elapsed_ms = began.elapsed().as_millis();
    crate::debug_log(&format!(
        "feed covers warm n={wanted} ok={} ms={elapsed_ms}",
        warm.count()
    ));
    if truncated {
        crate::debug_log(&format!(
            "feed covers warm truncated n={wanted} ok={} budget_ms={}",
            warm.count(),
            COVER_WARM_BUDGET.as_millis()
        ));
    }
    warm
}

/// The configured ring capacity (already normalised by settings::load, but a
/// later UI change may be in flight too, so clamp again).
pub(crate) fn feed_prefetch_capacity(settings_store: &Arc<Mutex<settings::Settings>>) -> usize {
    settings_store
        .lock()
        .map(|guard| guard.feed_prefetch_batches.min(settings::FEED_PREFETCH_MAX) as usize)
        .unwrap_or(0)
}

/// UI thread: consume one ready batch, if there is one.
pub(crate) fn take_prefetched_batch(
    ring: &Arc<Mutex<FeedPrefetchRing>>,
    source: i32,
) -> Option<PrefetchedFeedBatch> {
    ring.lock().ok().and_then(|mut guard| guard.promote(source))
}

/// Ring snapshot for the logs: ready count (after pruning) and next cursor.
pub(crate) fn feed_prefetch_state(ring: &Arc<Mutex<FeedPrefetchRing>>) -> (usize, u64) {
    ring.lock()
        .map(|mut guard| {
            guard.prune();
            (guard.ready.len(), guard.cursor())
        })
        .unwrap_or((0, 0))
}

/// Keep the ring filled with the configured number of batches. Runs on its own
/// thread from startup: it waits for a session (so it also runs while the login
/// screen is up), then fills one slot at a time and sleeps between checks.
pub(crate) fn spawn_feed_prefetch(
    session_slot: SessionSlot,
    ring: Arc<Mutex<FeedPrefetchRing>>,
    settings_store: Arc<Mutex<settings::Settings>>,
) {
    std::thread::spawn(move || {
        // Startup gate: while a player is open the ring neither starts a new
        // batch nor warms its covers. Both are pure background work, and both
        // compete with the first frames for the same link. Already-ready
        // batches stay queued and are promoted on refresh as usual.
        let gate = prefetch::startup_gate_enabled();
        let mut held = false;
        loop {
            if gate && player_active() {
                if !held {
                    held = true;
                    crate::debug_log("startup gate feed=hold reason=player-open");
                }
                std::thread::sleep(FEED_PREFETCH_POLL);
                continue;
            }
            if held {
                held = false;
                crate::debug_log("startup gate feed=start reason=player-closed");
            }
            let capacity = feed_prefetch_capacity(&settings_store);
            let Some(session) = active_session(&session_slot) else {
                std::thread::sleep(FEED_PREFETCH_POLL);
                continue;
            };
            // Reserve the cursor and mark the fill in flight under the same lock:
            // a rebase that lands after this point sees `in_flight` and can
            // invalidate this fetch before it is queued.
            let (ready, source, start, epoch, want_fill) = {
                let Ok(mut guard) = ring.lock() else {
                    std::thread::sleep(FEED_PREFETCH_POLL);
                    continue;
                };
                guard.switch_source(FEED_SOURCE_RECOMMEND);
                guard.prune();
                // A settings change can lower the cap while slots are queued.
                while guard.ready.len() > capacity {
                    guard.ready.pop_back();
                }
                let want_fill = capacity > 0 && guard.ready.len() < capacity;
                let start = guard.cursor();
                if want_fill {
                    guard.in_flight = Some(start);
                }
                (
                    guard.ready.len(),
                    guard.source,
                    start,
                    guard.epoch,
                    want_fill,
                )
            };
            if !want_fill {
                std::thread::sleep(FEED_PREFETCH_POLL);
                continue;
            }
            crate::debug_log(&format!(
                "feed prefetch refill started source={source} start={start} slot={}/{}",
                ready + 1,
                capacity
            ));
            let began = Instant::now();
            // Same fetch as the refresh path, only the progress callback is inert.
            let items = fetch_feed(session.clone(), start, Arc::new(|_, _| {}));
            let elapsed_ms = began.elapsed().as_millis();
            let count = items.len();
            // Warm the covers the first paint needs **before** the batch is queued,
            // so `ready` means "text and first screenful decoded". No ring lock is
            // held here (a refill may be promoted while it runs; `epoch` decides).
            let page_size = settings_store
                .lock()
                .map(|guard| guard.page_size)
                .unwrap_or_else(|_| settings::default_page_size());
            // Do not spend cover fetches on a batch that is already obsolete (a
            // rebase landed), no longer fits the ring, or whose covers would
            // compete with a player that opened while the fetch was in flight.
            let still_wanted = ring
                .lock()
                .map(|guard| guard.epoch == epoch && guard.ready.len() < capacity)
                .unwrap_or(false);
            let target = feed_cover_warm_target(page_size);
            let cap = items.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let player_holds = gate && player_active();
            let want_covers = !items.is_empty() && still_wanted && cover_warm_enabled();
            if want_covers && player_holds {
                crate::debug_log(&format!(
                    "startup gate feed=hold reason=player-open phase=covers start={start}"
                ));
            }
            let covers = if want_covers && !player_holds {
                warm_feed_covers(&session, &items, target)
            } else {
                WarmCovers::new(cap, target)
            };
            let covers_warm = covers.count();
            let covers_cap = covers.cap;
            // Text + warm-up: what "ready" now costs.
            let ready_ms = began.elapsed().as_millis();
            let outcome = {
                let Ok(mut guard) = ring.lock() else {
                    continue;
                };
                if guard.in_flight == Some(start) {
                    guard.in_flight = None;
                }
                if guard.epoch != epoch {
                    "rebase"
                } else if guard.ready.len() >= capacity {
                    "full"
                } else if items.is_empty() {
                    "empty"
                } else {
                    guard.next_start = start.saturating_add(bilibili::feed_batch().max(1));
                    guard.ready.push_back(PrefetchedFeedBatch {
                        source,
                        start,
                        items,
                        covers,
                        ready_at: Instant::now(),
                    });
                    guard.prune();
                    "ok"
                }
            };
            match outcome {
                "ok" => {
                    let (now_ready, _) = feed_prefetch_state(&ring);
                    crate::debug_log(&format!(
                        "feed prefetch refill ok source={source} start={start} items={count} ms={elapsed_ms}"
                    ));
                    crate::debug_log(&format!(
                        "feed prefetch ready={now_ready}/{capacity} covers={covers_warm}/{covers_cap} source={source} ms={ready_ms}"
                    ));
                    if cover_warm_enabled() && covers_warm == 0 && covers_cap > 0 && !player_active() {
                        crate::debug_log(&format!(
                            "feed prefetch refill covers=failed source={source} start={start}"
                        ));
                    }
                }
                "empty" => {
                    crate::debug_log(&format!(
                        "feed prefetch refill err=empty source={source} start={start} ms={elapsed_ms}"
                    ));
                    // Do not hammer a failing endpoint; the cursor stays put so the
                    // same slot is retried once the network recovers.
                    std::thread::sleep(Duration::from_secs(1));
                }
                reason => {
                    crate::debug_log(&format!(
                        "feed prefetch refill dropped source={source} start={start} reason={reason}"
                    ));
                }
            }
        }
    });
}

/// The covers `stream_feed_covers` has left for a batch: its cover cap minus
/// the rows a ring warm-up already decoded (a promoted batch repaints none of
/// its warm covers). Also returns the cap itself, the model's cover count.
pub(crate) fn pending_feed_covers(
    videos: &[bilibili::FeedVideo],
    warm: &[(usize, Arc<CoverPixels>)],
) -> (usize, Vec<PendingCover>) {
    let cap = videos.len().min(bilibili::THUMB_MAX_PER_VIEW);
    let warmed: HashSet<usize> = warm.iter().map(|(index, _)| *index).collect();
    let pending = videos
        .iter()
        .take(cap)
        .enumerate()
        .filter(|(index, _)| !warmed.contains(index))
        .map(|(index, video)| PendingCover {
            index,
            pic: video.pic.clone(),
            bvid: video.bvid.clone(),
        })
        .collect();
    (cap, pending)
}

/// Fetch only the recommendation JSON on a private thread with its own
/// runtime; the covers stream separately in `load_feed`.
pub(crate) fn fetch_feed(
    session: Arc<bilibili::Session>,
    fresh_start: u64,
    on_page: Arc<dyn Fn(u64, u64) + Send + Sync>,
) -> Vec<bilibili::FeedVideo> {
    let Some(runtime) = crate::bilibili::shared_runtime() else {
        return Vec::new();
    };
    runtime.block_on(async move {
        match session.homepage_feed_from(fresh_start, &*on_page).await {
            Ok(items) => items,
            Err(err) => {
                crate::debug_log(&format!("feed err={err}"));
                Vec::new()
            }
        }
    })
}

/// Fetch one feed batch (identified by its `fresh_idx` start) on a worker
/// thread. The 换一换 path and the page bar's first / prev / next / refresh all
/// land here; `target_page` is the client page the batch lands on (0 for a new
/// batch, the reloaded page for a refresh).
pub(crate) fn spawn_feed_load(
    weak: Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    session_slot: SessionSlot,
    feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    start: u64,
    target_page: i64,
) {
    crate::debug_log(&format!(
        "feed load spawn start={start} target_page={target_page}"
    ));
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        update_page_bar(&ui);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        let my = generation.load(Ordering::SeqCst);
        runtime.block_on(load_feed(
            &weak,
            &generation,
            my,
            &session_slot,
            &feed_store,
            start,
            target_page,
        ));
    }));
}

/// Fetch the single account's recommendation list and show it: the text rows
/// first (phase A), then the covers streamed into the rendered rows (phase B).
pub(crate) async fn load_feed(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session_slot: &SessionSlot,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    fresh_start: u64,
    target_page: i64,
) {
    let load_started = Instant::now();
    post(weak, generation, my, |ui| {
        publish_refresh(ui, true, 0.0);
        // The bar's refresh mark spins off the shared skeleton phase while the
        // load is in flight; this arm keeps that ticker alive until phase A.
        skeleton_arm(ui, 1);
        update_page_bar(ui);
    });

    let Some(session) = active_session(session_slot) else {
        post(weak, generation, my, |ui| {
            publish_refresh(ui, false, 1.0);
            ui.set_status("没有可用的账号会话".into());
            skeleton_settle(ui, 1);
            update_page_bar(ui);
        });
        return;
    };

    // The feed fetches `feed_batch()` pages in one call; the callback lands once
    // per page on the worker thread and republishes the bar as pages_done/total.
    let progress_weak = weak.clone();
    let progress_generation = generation.clone();
    let on_page: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(move |done, total| {
        crate::debug_log(&format!("refresh progress {done}/{total}"));
        let fraction = if total == 0 {
            1.0
        } else {
            done as f32 / total as f32
        };
        post(&progress_weak, &progress_generation, my, move |ui| {
            publish_refresh(ui, true, fraction);
        });
    });

    let videos = match std::thread::spawn({
        let session = session.clone();
        move || fetch_feed(session, fresh_start, on_page)
    })
    .join()
    {
        Ok(videos) => videos,
        Err(_) => Vec::new(),
    };
    crate::debug_log(&format!(
        "feed load fetched ms={} items={}",
        load_started.elapsed().as_millis(),
        videos.len()
    ));
    if videos.is_empty() {
        post(weak, generation, my, |ui| {
            publish_refresh(ui, false, 1.0);
            ui.set_status("首页推荐为空".into());
            skeleton_settle(ui, 1);
            update_page_bar(ui);
        });
        return;
    }

    let (cap, pending) = pending_feed_covers(&videos, &[]);
    let item_count = videos.len();

    // ---- Phase A: text rows on screen immediately ----
    let feed_store_a = feed_store.clone();
    let generation_a = generation.clone();
    let started_a = load_started;
    post(weak, generation, my, move |ui| {
        if publish_feed_rows(
            ui,
            &generation_a,
            my,
            &feed_store_a,
            videos,
            target_page,
            cap,
        ) {
            crate::debug_log(&format!(
                "feed load rendered ms={} items={item_count}",
                started_a.elapsed().as_millis()
            ));
        }
    });

    // ---- Phase B: each cover patches its own row as it arrives ----
    stream_feed_covers(
        weak,
        generation,
        my,
        &session,
        feed_store,
        pending,
        0,
        &format!("feed start={fresh_start}"),
    )
    .await;
}

/// UI thread: publish a promoted batch with its warm covers attached and stream
/// the rest from a background thread. Used by the home refresh, which must not
/// fetch on the UI path. Logs the promotion line itself, so `covers=` is the
/// count of covers that were already painted by this one event.
pub(crate) fn show_prefetched_batch(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session: Option<Arc<bilibili::Session>>,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    batch: PrefetchedFeedBatch,
    target_page: i64,
) -> bool {
    let began = Instant::now();
    let age_ms = batch.ready_at.elapsed().as_millis();
    let item_count = batch.items.len();
    let batch_start = batch.start;
    let PrefetchedFeedBatch {
        items, covers, ..
    } = batch;
    let warm_target = covers.target;
    let warm = covers.rows;
    let warm_count = warm.len();
    let cover_cap = covers.cap;
    let (cap, pending) = pending_feed_covers(&items, &warm);
    let pending_count = pending.len();
    // How many covers of the first screenful the display path still has to
    // fetch after this promotion: `0` is the whole point of the warm-up.
    let first_screen_pending = pending.iter().filter(|cover| cover.index < warm_target).count();
    let Some(ui) = weak.upgrade() else {
        return false;
    };
    if !publish_feed_rows_with_covers(&ui, generation, my, feed_store, items, target_page, cap, &warm)
    {
        return false;
    }
    crate::debug_log(&format!(
        "feed prefetch promote items={item_count} covers={warm_count}/{cover_cap} pending={pending_count} first_screen_pending={first_screen_pending} age_ms={age_ms} ms={} start={batch_start} path=ring",
        began.elapsed().as_millis()
    ));
    let Some(session) = session else {
        return true;
    };
    let weak_b = weak.clone();
    let generation_b = generation.clone();
    let feed_store_b = feed_store.clone();
    crate::netpool::submit(crate::netpool::Prio::Low, Box::new(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(stream_feed_covers(
            &weak_b,
            &generation_b,
            my,
            &session,
            &feed_store_b,
            pending,
            warm_target,
            "feed prefetch",
        ));
    }));
    true
}

/// UI thread: leave the login view for the populated home page.
pub(crate) fn reveal_home(ui: &AppWindow) {
    ui.set_logged_in(true);
    ui.set_view_mode(0);
    enter_paged_view(ui);
    update_page_bar(ui);
    HOME_SHOWN.store(true, Ordering::SeqCst);
}

/// Wait up to `timeout` for a ready batch of `source`, polling the ring. Uses
/// the async sleep so the shared runtime keeps its workers serving the network
/// while the login view waits.
pub(crate) async fn wait_prefetched_batch(
    ring: &Arc<Mutex<FeedPrefetchRing>>,
    source: i32,
    timeout: Duration,
    generation: &Arc<AtomicU64>,
    my: u64,
) -> StartupBatch {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(batch) = take_prefetched_batch(ring, source) {
            return StartupBatch::Ready(batch);
        }
        if generation.load(Ordering::SeqCst) != my {
            return StartupBatch::Superseded;
        }
        if Instant::now() >= deadline {
            return StartupBatch::Timeout;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// After a successful login: reveal the home view with a batch already on
/// screen (a), or after [`FEED_HOME_WAIT`] regardless (b), so the login screen
/// can never trap the user. The timeout path keeps today's network load and
/// rebases the ring past the cursor that load uses.
pub(crate) async fn show_home_after_login(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session_slot: &SessionSlot,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    ring: &Arc<Mutex<FeedPrefetchRing>>,
    session: Arc<bilibili::Session>,
    capacity: usize,
) {
    let waited = Instant::now();
    if capacity == 0 {
        // Preloading is off: nothing can arrive, so do not make the user wait.
        let elapsed = waited.elapsed().as_millis();
        post(weak, generation, my, move |ui| {
            reveal_home(ui);
            crate::debug_log(&format!(
                "startup home show waited_ms={elapsed} batch=disabled"
            ));
        });
        load_feed(
            weak,
            generation,
            my,
            session_slot,
            feed_store,
            FEED_START_CURSOR,
            0,
        )
        .await;
        return;
    }
    match wait_prefetched_batch(ring, FEED_SOURCE_RECOMMEND, FEED_HOME_WAIT, generation, my).await {
        StartupBatch::Superseded => {}
        StartupBatch::Ready(batch) => {
            let item_count = batch.items.len();
            let batch_start = batch.start;
            let warm = batch.covers.rows;
            let warm_count = warm.len();
            let cover_cap = batch.covers.cap;
            let (cap, pending) = pending_feed_covers(&batch.items, &warm);
            let feed_store_a = feed_store.clone();
            let generation_a = generation.clone();
            post(weak, generation, my, move |ui| {
                reveal_home(ui);
                let published = publish_feed_rows_with_covers(
                    ui,
                    &generation_a,
                    my,
                    &feed_store_a,
                    batch.items,
                    0,
                    cap,
                    &warm,
                );
                crate::debug_log(&format!(
                    "startup home show waited_ms={} batch=ready",
                    waited.elapsed().as_millis()
                ));
                if published {
                    crate::debug_log(&format!(
                        "startup feed items={item_count} start={batch_start} covers={warm_count}/{cover_cap}"
                    ));
                }
            });
            let weak_b = weak.clone();
            let generation_b = generation.clone();
            let feed_store_b = feed_store.clone();
            let session_b = session.clone();
            crate::netpool::submit(crate::netpool::Prio::Low, Box::new(move || {
                let Some(runtime) = bilibili::shared_runtime() else {
                    return;
                };
                runtime.block_on(stream_feed_covers(
                    &weak_b,
                    &generation_b,
                    my,
                    &session_b,
                    &feed_store_b,
                    pending,
                    0,
                    &format!("feed start={batch_start}"),
                ));
            }));
        }
        StartupBatch::Timeout => {
            // Show the home view now and let the existing network path fill it.
            let elapsed = waited.elapsed().as_millis();
            post(weak, generation, my, move |ui| {
                reveal_home(ui);
                crate::debug_log(&format!(
                    "startup home show waited_ms={elapsed} batch=timeout"
                ));
            });
            // That load uses the startup cursor, so a queued/in-flight copy of
            // it would be the same content twice.
            if let Ok(mut guard) = ring.lock() {
                guard.rebase_past(FEED_START_CURSOR);
            }
            load_feed(
                weak,
                generation,
                my,
                session_slot,
                feed_store,
                FEED_START_CURSOR,
                0,
            )
            .await;
        }
    }
}

/// UI hand-off of the covers a stream never delivered (worker cut short);
/// keeps the skeleton counter exact and closes the patch summary.
pub(crate) fn flush_missing(weak: &Weak<AppWindow>, sync: &Arc<ThumbSync>, missing: usize) {
    if missing == 0 {
        return;
    }
    let weak = weak.clone();
    let sync = sync.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            skeleton_settle(&ui, missing as u64);
            for _ in 0..missing {
                sync.settle(false);
            }
        }
    });
}

/// Phase B for one already-rendered `T`-row list: stream `pics` with bounded
/// concurrency and patch each decoded cover into the row at
/// `index_offset + index`. `keys` holds the per-row bvid / dynamic id;
/// `guard_of` builds the identity check that discards a patch whose load was
/// superseded, so stale covers can never land on newer rows.
pub(crate) async fn stream_cover_list<T: Clone + 'static, G>(
    weak: &Weak<AppWindow>,
    session: &Arc<bilibili::Session>,
    pics: Vec<String>,
    keys: Vec<String>,
    index_offset: usize,
    mut guard_of: G,
    get_model: fn(&AppWindow) -> ModelRc<T>,
    set_thumb: fn(&mut T, Image),
    label: String,
) where
    G: FnMut(usize, String) -> ThumbGuard,
{
    let total = pics.len();
    let sync = ThumbSync::new(label.clone(), total);
    let started = Instant::now();
    let fallbacks_before = bilibili::thumb_fallbacks();
    let mut delivered = 0usize;
    stream_thumbs(session, pics, |_| {}, |index, pixels| {
        delivered += 1;
        let row = index_offset + index;
        let key = keys.get(index).cloned().unwrap_or_default();
        post_thumb_patch(
            weak,
            row,
            pixels,
            guard_of(row, key),
            get_model,
            set_thumb,
            sync.clone(),
        );
    })
    .await;
    crate::debug_log(&format!(
        "{label} thumbs n={} fallback={} ms={}",
        delivered,
        bilibili::thumb_fallbacks().saturating_sub(fallbacks_before),
        started.elapsed().as_millis()
    ));
    flush_missing(weak, &sync, total.saturating_sub(delivered));
}

/// Phase B for a shared `[CardRow]` list (分区 / 排行): stream the
/// covers of the first `THUMB_MAX_PER_VIEW` cards.
pub(crate) async fn stream_card_covers(
    weak: &Weak<AppWindow>,
    session: &Arc<bilibili::Session>,
    cards: &[bilibili::VideoCard],
    ids: Arc<Mutex<Vec<String>>>,
    get_model: fn(&AppWindow) -> ModelRc<CardRow>,
    label: String,
) {
    let cap = cards.len().min(bilibili::THUMB_MAX_PER_VIEW);
    let pics: Vec<String> = cards.iter().take(cap).map(|card| card.pic.clone()).collect();
    let bvids: Vec<String> = cards
        .iter()
        .take(cap)
        .map(|card| card.bvid.clone())
        .collect();
    stream_cover_list(
        weak,
        session,
        pics,
        bvids,
        0,
        move |_index, key| ThumbGuard::Ids(ids.clone(), key),
        get_model,
        |row, image| row.thumb = image,
        label,
    )
    .await;
}

/// Build the UI row for a decoded card (creates `slint::Image`; UI thread only).
pub(crate) fn build_card_row(row: &CardRowData) -> CardRow {
    CardRow {
        thumb: to_thumb(&row.pixels),
        title: row.title.clone().into(),
        meta: row.meta.clone().into(),
        badge: row.badge.clone().into(),
    }
}

/// Push a card list's bvids for click routing and build its UI rows.
pub(crate) fn publish_cards(
    rows: &[(String, CardRowData)],
    browse_bvids: &Arc<Mutex<Vec<String>>>,
) -> Vec<CardRow> {
    let bvids: Vec<String> = rows.iter().map(|(bvid, _)| bvid.clone()).collect();
    if let Ok(mut guard) = browse_bvids.lock() {
        *guard = bvids;
    }
    rows.iter().map(|(_, row)| build_card_row(row)).collect()
}

pub(crate) fn load_region(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    region_state: Arc<Mutex<RegionState>>,
    browse_bvids: Arc<Mutex<Vec<String>>>,
    page: i64,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (rid, name, page) = match region_state.lock() {
        Ok(mut state) => {
            state.page = page.max(1);
            (state.rid, state.name.clone(), state.page)
        }
        Err(_) => return,
    };
    // One API page per screen: fetch with the live client page size so the
    // loaded page fills exactly one screen (the endpoint's default applies when
    // the size is out of range).
    let ps = current_page_size(&weak);
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_region_title(name.into());
        ui.set_region_current_page(page as i32);
        update_page_bar(&ui);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let card_page = match session.region_latest(rid, page, ps).await {
                Ok(card_page) => card_page,
                Err(err) => {
                    crate::debug_log(&format!("region rid={rid} err={err:#}"));
                    let message = format!("分区视频加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_region_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                Vec::<CardRow>::new(),
                            ))));
                            ui.set_region_page_text(message.into());
                            ui.set_region_has_prev(false);
                            ui.set_region_has_next(false);
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let num = card_page.page;
            let has_more = card_page.has_more;
            let cards = card_page.items;
            let rows: Vec<(String, CardRowData)> = cards
                .iter()
                .map(|card| {
                    let meta = format!(
                        "{} · {}播放 · {}",
                        card.owner,
                        format_view(card.view),
                        format_duration(card.duration)
                    );
                    (
                        card.bvid.clone(),
                        CardRowData {
                            pixels: None,
                            title: card.title.clone(),
                            meta,
                            badge: format_duration(card.duration),
                        },
                    )
                })
                .collect();
            let loaded = rows.len();
            let covers = cards.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let weak_a = weak.clone();
            let browse_a = browse_bvids.clone();
            let region_state_a = region_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let items = publish_cards(&rows, &browse_a);
                    if let Ok(mut state) = region_state_a.lock() {
                        state.page = num;
                        state.has_more = has_more;
                    }
                    ui.set_region_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_region_page_text(format!("第 {num} 页").into());
                    ui.set_region_current_page(num as i32);
                    ui.set_region_has_prev(num > 1);
                    ui.set_region_has_next(has_more);
                    // One API page is one screen: the offset restarts at 0.
                    set_client_page(8, 0);
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("region load rendered items={loaded}"));
                }
            });
            stream_card_covers(
                &weak,
                &session,
                &cards,
                browse_bvids,
                |ui| ui.get_region_items(),
                format!("region rid={rid} page={num}"),
            )
            .await;
        });
    }));
}

/// Load one page of the watch history and (append to) the card list.
///
/// `append` loads the next cursor page on top of the loaded rows; `target_page`
/// is the client page the list lands on afterwards (`None` keeps the current
/// one, and a non-append load restarts at 0 because the data is replaced).
pub(crate) fn load_history(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    history_page: Arc<Mutex<HistoryPageState>>,
    history_kids: Arc<Mutex<Vec<String>>>,
    history_bvids: Arc<Mutex<Vec<String>>>,
    append: bool,
    target_page: Option<i64>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (max, view_at, business) = match history_page.lock() {
        Ok(state) => (state.max, state.view_at, state.business.clone()),
        Err(_) => return,
    };
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        update_page_bar(&ui);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let page = match session.history(max, view_at, &business).await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!("history err={err:#}"));
                    let message = format!("历史记录加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_history_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };

            let items = page.items;
            let pics: Vec<String> = items.iter().map(|item| item.cover.clone()).collect();
            let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<(String, String, HistoryRowData)> = items
                .iter()
                .map(|item| {
                    let progress = item.progress.max(0) as u64;
                    let meta = format!(
                        "{} · {} / {}",
                        item.author,
                        format_duration(progress),
                        format_duration(item.duration)
                    );
                    (
                        item.bvid.clone(),
                        item.kid.clone(),
                        HistoryRowData {
                            pixels: None,
                            title: item.title.clone(),
                            meta,
                            badge: format_duration(item.duration),
                        },
                    )
                })
                .collect();
            let mids: Vec<i64> = items.iter().map(|item| item.owner_mid).collect();
            let cursor = page.cursor;
            let has_more = page.has_more;
            let base_len = if append {
                history_bvids.lock().map(|guard| guard.len()).unwrap_or(0)
            } else {
                0
            };
            let loaded = rows.len();
            let weak_a = weak.clone();
            let history_bvids_a = history_bvids.clone();
            let history_kids_a = history_kids.clone();
            let history_page_a = history_page.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let mut all_rows: Vec<HistoryRow> = if append {
                        let model = ui.get_history_items();
                        (0..model.row_count())
                            .filter_map(|index| model.row_data(index))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let mut all_bvids: Vec<String> = if append {
                        history_bvids_a
                            .lock()
                            .map(|guard| guard.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let mut all_kids: Vec<String> = if append {
                        history_kids_a
                            .lock()
                            .map(|guard| guard.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let mut all_mids: Vec<i64> = if append {
                        history_page_a
                            .lock()
                            .map(|state| state.mids.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    for (bvid, kid, row) in rows {
                        all_rows.push(HistoryRow {
                            thumb: to_thumb(&row.pixels),
                            title: row.title.into(),
                            meta: row.meta.into(),
                            badge: row.badge.into(),
                        });
                        all_bvids.push(bvid);
                        all_kids.push(kid);
                    }
                    all_mids.extend(mids);
                    if let Ok(mut guard) = history_bvids_a.lock() {
                        *guard = all_bvids;
                    }
                    if let Ok(mut guard) = history_kids_a.lock() {
                        *guard = all_kids;
                    }
                    if let Ok(mut state) = history_page_a.lock() {
                        if let Some(cursor) = &cursor {
                            state.max = cursor.max;
                            state.view_at = cursor.view_at;
                            state.business = cursor.business.clone();
                        } else {
                            state.max = 0;
                        }
                        state.has_more = has_more;
                        state.mids = all_mids;
                    }
                    // The bar's counter is the client page over the loaded rows:
                    // a reset reload starts at 0, an append lands on the page
                    // the bar asked for (or keeps the current one).
                    if !append {
                        set_client_page(5, 0);
                    } else if let Some(target) = target_page {
                        set_client_page(5, target);
                    }
                    ui.set_history_items(ModelRc::from(std::rc::Rc::new(VecModel::from(all_rows))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_history_has_more(has_more);
                    ui.set_history_status(String::new().into());
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("history load rendered items={loaded}"));
                }
            });
            let keys: Vec<String> = items
                .iter()
                .take(covers)
                .map(|item| item.bvid.clone())
                .collect();
            let mut pics = pics;
            pics.truncate(covers);
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                base_len,
                move |_index, key| ThumbGuard::Ids(history_bvids.clone(), key),
                |ui| ui.get_history_items(),
                |row, image| row.thumb = image,
                format!("history append={append}"),
            )
            .await;
        });
    }));
}

pub(crate) fn load_toview(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    toview_aids: Arc<Mutex<Vec<i64>>>,
    toview_bvids: Arc<Mutex<Vec<String>>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_toview_status(String::new().into());
        update_page_bar(&ui);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let page = match session.toview().await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!("toview err={err:#}"));
                    let message = format!("稍后再看加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_toview_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };

            let items = page.items;
            let pics: Vec<String> = items.iter().map(|item| item.cover.clone()).collect();
            let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);

            let rows: Vec<(i64, String, ToViewRowData)> = items
                .iter()
                .map(|item| {
                    let meta = format!(
                        "{} · {}播放 · {}",
                        item.owner,
                        format_view(item.view),
                        format_duration(item.duration)
                    );
                    (
                        item.aid,
                        item.bvid.clone(),
                        ToViewRowData {
                            pixels: None,
                            title: item.title.clone(),
                            meta,
                            badge: format_duration(item.duration),
                        },
                    )
                })
                .collect();
            let loaded = rows.len();
            // The endpoint's own total for the list (diagnostic only now that the
            // "共 N 个视频" status line is gone).
            let count = page.count;
            let weak_a = weak.clone();
            let toview_aids_a = toview_aids.clone();
            let toview_bvids_a = toview_bvids.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let mut aids = Vec::with_capacity(rows.len());
                    let mut bvids = Vec::with_capacity(rows.len());
                    let items: Vec<ToViewRow> = rows
                        .iter()
                        .map(|(aid, bvid, row)| {
                            aids.push(*aid);
                            bvids.push(bvid.clone());
                            ToViewRow {
                                thumb: to_thumb(&row.pixels),
                                title: row.title.clone().into(),
                                meta: row.meta.clone().into(),
                                badge: row.badge.clone().into(),
                            }
                        })
                        .collect();
                    if let Ok(mut guard) = toview_aids_a.lock() {
                        *guard = aids;
                    }
                    if let Ok(mut guard) = toview_bvids_a.lock() {
                        *guard = bvids;
                    }
                    ui.set_toview_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_toview_status(String::new().into());
                    // Single-shot list: the client page spans the loaded rows.
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("toview load rendered items={loaded} total={count}"));
                }
            });
            let keys: Vec<String> = items
                .iter()
                .take(covers)
                .map(|item| item.bvid.clone())
                .collect();
            let mut pics = pics;
            pics.truncate(covers);
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                0,
                move |_index, key| ThumbGuard::Ids(toview_bvids.clone(), key),
                |ui| ui.get_toview_items(),
                |row, image| row.thumb = image,
                "toview".to_string(),
            )
            .await;
        });
    }));
}

/// Fetch one video's detail + related list and present the detail view.
pub(crate) fn open_detail(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    detail_state: Arc<Mutex<Option<DetailState>>>,
    detail_seq: Arc<AtomicU64>,
    _related_bvids: Arc<Mutex<Vec<String>>>,
    bvid: String,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let seq = detail_seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(ui) = weak.upgrade() {
        // Card clicks resolve the video and open the player directly; the
        // detail page no longer opens, only the part selection is reset.
        ui.set_detail_selected_part(0);
    }

    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let detail = match session.video_view(&bvid).await {
                Ok(detail) => detail,
                Err(err) => {
                    let message = format!("视频详情加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if detail_seq.load(Ordering::SeqCst) != seq {
                            return;
                        }
                        if let Some(ui) = weak.upgrade() {
                            ui.set_detail_loading(false);
                            ui.set_detail_desc(message.into());
                        }
                    });
                    return;
                }
            };

            let mut cids: Vec<u64> = detail
                .pages
                .iter()
                .map(|page| page.cid.max(0) as u64)
                .collect();
            if cids.is_empty() {
                cids.push(detail.cid.max(0) as u64);
            }
            let state = DetailState {
                bvid: detail.bvid.clone(),
                title: detail.title.clone(),
                cid: detail.cid.max(0) as u64,
                aid: detail.aid,
                parts: cids,
            };

            crate::debug_log(&format!("open_detail->play bvid={}", detail.bvid));

            let _ = slint::invoke_from_event_loop(move || {
                if detail_seq.load(Ordering::SeqCst) != seq {
                    return;
                }
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut guard) = detail_state.lock() {
                        *guard = Some(state);
                    }
                    ui.invoke_play_detail();
                }
            });
        });
    }));
}

/// A cover for `url` when it is already decoded; a tinted block otherwise.
pub(crate) fn cached_cover(url: &str) -> Image {
    if url.is_empty() {
        return Image::default();
    }
    match cover_cache().lock() {
        Ok(mut cache) => cache
            .get(url)
            .map(|pixels| to_thumb_shared(&pixels))
            .unwrap_or_default(),
        Err(_) => Image::default(),
    }
}

/// Encode `text` as a QR code and rasterize it to raw RGBA8.
///
/// The module size is chosen so the code is displayed at an exact integer
/// number of device pixels per module (no fractional scaling of modules).
pub(crate) fn build_qr_rgba(text: &str) -> Result<(Vec<u8>, u32, u32)> {
    let code =
        qrcode::QrCode::new(text.as_bytes()).map_err(|e| anyhow!("生成二维码失败：{e:?}"))?;
    let modules = code.width();
    let quiet = 4usize;

    let target = 320usize;
    let scale = (target / (modules + quiet * 2)).clamp(2, 12);
    let side = (modules + quiet * 2) * scale;

    // White background, opaque.
    let mut buffer = vec![255u8; side * side * 4];

    let colors = code.to_colors();
    for (index, color) in colors.iter().enumerate() {
        if *color == qrcode::Color::Dark {
            let mx = index % modules;
            let my = index / modules;
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = (mx + quiet) * scale + dx;
                    let y = (my + quiet) * scale + dy;
                    let offset = (y * side + x) * 4;
                    buffer[offset] = 0;
                    buffer[offset + 1] = 0;
                    buffer[offset + 2] = 0;
                    buffer[offset + 3] = 255;
                }
            }
        }
    }

    Ok((buffer, side as u32, side as u32))
}

#[cfg(test)]
mod warm_tests {
    use super::*;

    #[test]
    pub(crate) fn every_tab_is_queued_and_a_hidden_live_tab_is_skipped() {
        assert_eq!(WARM_TABS.len(), 8, "one task per tab, 8/8 in the log");
        let labels: Vec<&str> = WARM_TABS.iter().map(|tab| tab.label()).collect();
        assert_eq!(
            labels,
            vec![
                "fav", "history", "toview", "region", "pgc", "live", "dynamic", "messages"
            ]
        );
        let mut prefs = settings::Settings::default();
        assert!(WARM_TABS.iter().all(|tab| tab.enabled(&prefs)));
        prefs.hide_live_tab = true;
        let kept: Vec<&str> = WARM_TABS
            .iter()
            .filter(|tab| tab.enabled(&prefs))
            .map(|tab| tab.label())
            .collect();
        assert_eq!(kept.len(), 7);
        assert!(!kept.contains(&"live"), "hide_live_tab skips the task");
    }

    #[test]
    pub(crate) fn the_queue_is_bounded_and_starts_after_the_first_screen() {
        assert_eq!(WARM_CONCURRENCY, 2, "two tabs at a time");
        assert_eq!(WARM_COVERS, 12, "first screen only");
        assert_eq!(WARM_TASK_LIMIT, Duration::from_secs(15));
        assert_eq!(WARM_START_DELAY, Duration::from_secs(2));
    }

    /// Cold vs warm first screen, tab by tab, against the real API and CDN.
    ///
    /// ```powershell
    /// $env:BILI_DEBUG_LOG="warm-bench.log"
    /// cargo test --release -- --ignored --nocapture live_warm_first_screen_benchmark
    /// ```
    ///
    /// Per tab it logs
    /// `warm bench tab=… api_ms=… items=… cold_ms=… cold_covers=… warm_ms=…
    /// hit_ms=… hit_covers=… cached=…`: the API page (same both ways — content
    /// must stay current), the covers fetched cold for the first screen, the
    /// queue's own cost, and the same first screen after the queue ran.
    #[test]
    #[ignore = "network: measures cold vs warm first-screen covers for every tab"]
    pub(crate) fn live_warm_first_screen_benchmark() {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        let use_account = std::env::var("BILI_PREFETCH_ACCOUNT")
            .map(|value| value.trim() == "1")
            .unwrap_or(false);
        let session = match if use_account {
            crate::accounts::load().map(|account| {
                let token = crate::accounts::to_token(&account);
                runtime.block_on(crate::bilibili::Session::from_token(&token))
            })
        } else {
            Some(runtime.block_on(crate::bilibili::Session::new()))
        } {
            Some(Ok(session)) => Arc::new(session),
            _ => {
                crate::debug_log("warm bench skipped reason=no-session");
                return;
            }
        };
        for tab in WARM_TABS {
            if !tab.enabled(&settings::Settings::default()) {
                continue;
            }
            // 1. the API page (the part warm cannot make faster).
            let at = Instant::now();
            let (items, pics) = runtime.block_on(warm_page(&session, tab));
            let api_ms = at.elapsed().as_millis();
            if pics.is_empty() {
                crate::debug_log(&format!(
                    "warm bench tab={} api_ms={api_ms} items={items} covers=0 reason=no-covers",
                    tab.label()
                ));
                continue;
            }
            // 2. what the queue pays: fetch this first screen into the cache.
            cover_cache_clear();
            let seen = Arc::new(Mutex::new(HashSet::new()));
            let at = Instant::now();
            let (fill_covers, _, _) = runtime.block_on(warm_covers(&session, tab, &pics, &seen));
            let fill_ms = at.elapsed().as_millis();
            // 3. what opening the tab pays after the queue ran: cache hits.
            let seen = Arc::new(Mutex::new(HashSet::new()));
            let at = Instant::now();
            let (hit_covers, hit_cached, _) =
                runtime.block_on(warm_covers(&session, tab, &pics, &seen));
            let hit_ms = at.elapsed().as_millis();
            // 4. what opening the tab pays without the queue: same covers, cold.
            cover_cache_clear();
            let seen = Arc::new(Mutex::new(HashSet::new()));
            let at = Instant::now();
            let (cold_covers, _, _) = runtime.block_on(warm_covers(&session, tab, &pics, &seen));
            let cold_ms = at.elapsed().as_millis();
            crate::debug_log(&format!(
                "warm bench tab={} api_ms={api_ms} items={items} cold_ms={cold_ms} cold_covers={cold_covers} fill_ms={fill_ms} fill_covers={fill_covers} hit_ms={hit_ms} hit_covers={hit_covers} hit_cached={hit_cached}",
                tab.label()
            ));
            println!(
                "warm bench tab={} api_ms={api_ms} items={items} cold_ms={cold_ms} cold_covers={cold_covers} hit_ms={hit_ms} hit_cached={hit_cached}",
                tab.label()
            );
        }
    }
}

#[cfg(test)]
mod cover_warm_tests {
    use super::*;

    pub(crate) fn video(index: usize) -> bilibili::FeedVideo {
        bilibili::FeedVideo {
            bvid: format!("BV{index}"),
            pic: format!("https://example.invalid/{index}.jpg"),
            ..bilibili::FeedVideo::default()
        }
    }

    pub(crate) fn cover(bytes: usize) -> Arc<CoverPixels> {
        Arc::new((vec![0u8; bytes], 1, 1))
    }

    #[test]
    pub(crate) fn warm_target_is_one_page_plus_margin_capped() {
        // A 12-card page plus the next row; a full 24-card page hits the cap.
        assert_eq!(feed_cover_warm_target(12), 16);
        assert_eq!(feed_cover_warm_target(24), COVER_WARM_MAX);
        assert_eq!(feed_cover_warm_target(48), COVER_WARM_MAX);
        // Defensive: a nonsensical page size still warms a small prefix.
        assert_eq!(feed_cover_warm_target(0), 1 + COVER_WARM_MARGIN);
    }

    #[test]
    pub(crate) fn pending_covers_skips_warm_rows_by_index() {
        let videos: Vec<bilibili::FeedVideo> = (0..5).map(video).collect();
        let warm = vec![(0, cover(4)), (2, cover(4))];
        let (cap, pending) = pending_feed_covers(&videos, &warm);
        assert_eq!(cap, 5);
        assert_eq!(
            pending.iter().map(|cover| cover.index).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
        assert_eq!(pending[0].bvid, "BV1");
        assert_eq!(pending[0].pic, "https://example.invalid/1.jpg");
    }

    #[test]
    pub(crate) fn pending_covers_caps_at_the_thumb_limit() {
        let videos: Vec<bilibili::FeedVideo> =
            (0..bilibili::THUMB_MAX_PER_VIEW + 5).map(video).collect();
        let (cap, pending) = pending_feed_covers(&videos, &[]);
        assert_eq!(cap, bilibili::THUMB_MAX_PER_VIEW);
        assert_eq!(pending.len(), bilibili::THUMB_MAX_PER_VIEW);
    }

    #[test]
    pub(crate) fn cover_cache_hit_returns_the_same_decode() {
        let mut cache = CoverCache::new();
        let pixels = cover(16);
        cache.put("a", pixels.clone());
        let hit = cache.get("a");
        assert!(hit.is_some());
        if let Some(hit) = hit {
            assert!(Arc::ptr_eq(&hit, &pixels));
        }
    }

    #[test]
    pub(crate) fn cover_cache_evicts_least_recently_used() {
        let mut cache = CoverCache::new();
        for index in 0..COVER_CACHE_ENTRIES {
            cache.put(&format!("k{index}"), cover(2));
        }
        // A hit makes the oldest entry the most recent, so its neighbour goes.
        assert!(cache.get("k0").is_some());
        cache.put("fresh", cover(2));
        assert_eq!(cache.entries.len(), COVER_CACHE_ENTRIES);
        assert!(cache.get("k1").is_none());
        assert!(cache.get("k0").is_some());
        assert!(cache.get("fresh").is_some());
    }

    #[test]
    pub(crate) fn cover_cache_ignores_empty_and_oversized_decodes() {
        let mut cache = CoverCache::new();
        cache.put("empty", cover(0));
        cache.put("huge", Arc::new((vec![0u8; COVER_CACHE_BYTES + 1], 1, 1)));
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }
}
