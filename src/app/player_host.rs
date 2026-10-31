//! The player window host: open/close, layout, panels and the comment drawer.

use crate::*;

/// The danmaku lane's rate for a player whose pacer may run at `out_hz`.
///
/// Without interpolation (`out_hz == 0`) the lane keeps the display's own rate:
/// a 30 fps picture needs its own high-rate overlay to look smooth, and that is
/// the shipped behaviour. With interpolation on, the window already repaints
/// `out_hz` times a second, and every danmaku row write is one more full-window
/// repaint, so the lane caps at 60 Hz: the comments keep their px/ms speed and
/// the repaint count stops doubling. Measured at 60 fps output on a 120 Hz
/// display, that is the difference between ~35 and ~55 pictures a second on a
/// busy host — the danmaku lane was asking for twice the repaints the picture
/// did.
pub(crate) fn danmaku_target_hz(out_hz: u32, display_hz: u32) -> u32 {
    if out_hz >= 60 {
        // An interpolated grid already repaints this window `out_hz` times a
        // second; a 120 Hz comment lane on top of it doubles that count for
        // half a pixel of movement per tick. 60 keeps the px/ms speed exactly
        // and lets the comment repaints share the picture's.
        display_hz.clamp(30, 60)
    } else {
        display_hz.clamp(30, 120)
    }
}

// ---------------------------------------------------------------------------
// 性能面板: the per-second instrument.
//
// The panel this replaced read `Stats::fps_presented` and printed it as
// 呈现 - but that counter is the **pacer's pushes**, not the frames that
// reached the screen. Measured on a 60 fps grid with 弹幕 on: 60 pushes,
// 29 presents, the rest merged by the mailbox. The panel said 120 because it
// was reporting the pacer's target, not the display.
//
// So every row below is a counter incremented by the thread that owns the
// event, read once a second, and nothing is derived from a target:
//
//   生成     `PlaybackControl::presented_frames` - the pacer's hand-offs.
//   上屏     the present-interval histogram's sample count - one sample per
//            frame that *actually* reached the screen (both present paths
//            record it: the DComp video sink and the Slint hand-over).
//   合并     生成 - 上屏 over the same window: a pushed frame is either shown
//            or replaced before it was, so this difference IS the coalesced
//            count, not an estimate.
//   帧间隔   p50/p95 of the same histogram, in ms.
//   解码     `Stats::fps_decoded` - frames ffmpeg's reader actually read.
//   卡顿     `Stats::stalls` - the watchdog's cumulative empty-ring stalls.
//   弹幕帧率  the fast lane's own tick counter (one tick = one repaint
//            request), counted in the callback that runs it.
//   绘制     the layer's `DanmakuStats::render_ms` p50/p95. Only the DComp
//            comment layer has one: on the winit path the comments are drawn
//            inside the UI tree, so the row says "—" instead of inventing a
//            cost the process cannot measure.
//   UI 间隔  the 4 ms probe in the 性能 body: real gaps on this event loop.
//   UI 绘制  `dcomp::ui_counters` - the last UI paint's cost.
//   速率     the proxy's counted bytes/s (no ≈), plus this session's peak.
//   连接     busy workers / configured ones, from the same snapshot.
//   缓冲     decoded seconds ahead, and the ring's occupancy at the track's
//            own rate.
//   失败区间 `PrefetchStats::failed_ranges`.
//   插帧     模式 / 档位 / 一帧的成本 / 视频层达到的上屏率.
//
// Values are formatted here, once a second: an empty one is "—" and never a
// zero pretending to be a measurement. The write only happens while the player
// window is on screen and playing - a paused player publishes nothing, so the
// panel keeps the last second of playback it measured instead of decaying to
// zero.
// ---------------------------------------------------------------------------

/// Samples one rolling cost window keeps. The UI probe runs at 4 ms, so 256
/// samples is the last second; the 弹幕 windows fill slower and therefore span
/// more than a second, which is what their p50/p95 wants.
const PERF_HIST_SAMPLES: usize = 256;

/// The 性能 panel publishes once a second. Anything faster would turn the
/// panel into a per-frame property writer (the old 4 Hz `网络分析` publish is
/// still there for the strip; this one is the instrument).
const PERF_PUBLISH_MS: i64 = 1000;

/// Rolling sample window for one measured cost, in 1/100 ms.
#[derive(Default)]
struct CostHist {
    samples: VecDeque<u16>,
}

impl CostHist {
    /// Add one sample. Non-finite/negative values are dropped: a cost that
    /// cannot be represented is not a measurement.
    fn push(&mut self, ms: f32) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        if self.samples.len() >= PERF_HIST_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((ms * 100.0).min(f32::from(u16::MAX)) as u16);
    }

    /// `(p50, p95)` in ms, or `None` while there is nothing to describe (one
    /// sample is not a distribution, so the row reads "—" until there are two).
    fn percentiles(&self) -> Option<(f64, f64)> {
        if self.samples.len() < 2 {
            return None;
        }
        let mut values: Vec<u16> = self.samples.iter().copied().collect();
        values.sort_unstable();
        let pick = |percent: f64| -> f64 {
            let index = ((values.len() as f64 * percent).ceil() as usize)
                .saturating_sub(1)
                .min(values.len() - 1);
            f64::from(values[index]) / 100.0
        };
        Some((pick(0.50), pick(0.95)))
    }
}

/// One publish's playback state, read by the tick that publishes (`None` rows
/// need to know why a counter has no sample).
#[derive(Clone, Copy)]
pub(crate) struct PerfCtx {
    pub(crate) danmaku_on: bool,
    pub(crate) paused: bool,
    pub(crate) frames_on: bool,
    /// Rows the danmaku model currently holds.
    pub(crate) dm_rows: usize,
}

/// The 性能 panel's measurements. One per player window, UI thread only.
pub(crate) struct PerfPanel {
    /// Player-clock ms of the previous publish (`0` = none yet, so the first
    /// window is measured from the player's own start).
    last_at: Cell<i64>,
    /// Cumulative counters at the previous publish.
    last_pushed: Cell<u64>,
    last_shown: Cell<u64>,
    last_dm_ticks: Cell<u64>,
    /// Highest metered throughput of this session, bytes/s.
    peak_bps: Cell<u64>,
    /// 弹幕 fast-lane ticks, shared with the lane's own callback.
    dm_ticks: Rc<Cell<u64>>,
    /// Cost windows: 弹幕 layer render, UI probe gap.
    dm_render_ms: RefCell<CostHist>,
    ui_gap_ms: RefCell<CostHist>,
    /// Monotonic instant of the previous 4 ms probe callback.
    ui_last: Cell<Option<Instant>>,
    /// Gate for the 20 Hz 弹幕 render sample: reading that counter takes the
    /// layer registry lock, so it is not read per frame.
    dm_sample_at: Cell<i64>,
}

impl PerfPanel {
    pub(crate) fn new() -> Self {
        Self {
            last_at: Cell::new(0),
            last_pushed: Cell::new(0),
            last_shown: Cell::new(0),
            last_dm_ticks: Cell::new(0),
            peak_bps: Cell::new(0),
            dm_ticks: Rc::new(Cell::new(0)),
            dm_render_ms: RefCell::new(CostHist::default()),
            ui_gap_ms: RefCell::new(CostHist::default()),
            ui_last: Cell::new(None),
            dm_sample_at: Cell::new(-50),
        }
    }

    /// One 弹幕 fast-lane tick: one repaint request by the lane. The lane's
    /// own gate drops the wakes that arrive early, so this is the rate the
    /// tick scheduler really ran at - the number the row reports.
    pub(crate) fn note_dm_tick(&self) {
        self.dm_ticks.set(self.dm_ticks.get().wrapping_add(1));
    }

    /// One sample of the 弹幕 layer's per-frame render cost (DComp path).
    pub(crate) fn note_dm_render(&self, ms: f32) {
        self.dm_render_ms.borrow_mut().push(ms);
    }

    /// One 4 ms probe callback: the gap since the previous one is the event
    /// loop's real cadence while the panel is on screen.
    pub(crate) fn note_ui_tick(&self) {
        let now = Instant::now();
        if let Some(previous) = self.ui_last.replace(Some(now)) {
            self.ui_gap_ms
                .borrow_mut()
                .push(now.duration_since(previous).as_secs_f32() * 1000.0);
        }
    }

    /// Publish one second of measurements. `now_ms` is the player tick's clock.
    pub(crate) fn publish(
        &self,
        window: &PlayerWindow,
        stats: Option<&player::Stats>,
        metered: Option<&prefetch::PrefetchStats>,
        control: &player::PlaybackControl,
        ctx: PerfCtx,
        now_ms: i64,
    ) {
        // 暂停时不发布: a frozen picture's numbers freeze with it, so the panel
        // keeps the last second of playback it measured instead of decaying
        // into 0.0 /s a second after the pause. The one exception is a player
        // that is already paused before it ever published (opened paused): it
        // gets one snapshot, or every row would read "—" forever.
        if ctx.paused && self.last_at.get() != 0 {
            return;
        }
        // A hidden window is never published to.
        if control.occluded() {
            return;
        }

        // Video: the two counters are cumulative for this player window, so the
        // rate is the delta over the window this publish covers.
        let pushed = control.presented_frames();
        let shown = control.present_percentiles().2;
        let ticks = self.dm_ticks.get();
        let previous_at = self.last_at.replace(now_ms);
        let dt_ms = now_ms - previous_at;
        if previous_at != 0 && dt_ms > 2500 {
            // A gap (a pause, a hidden window, a long seek): the deltas over it
            // do not describe one second, so the window restarts here and the
            // next publish is a clean one.
            self.last_pushed.set(pushed);
            self.last_shown.set(shown);
            self.last_dm_ticks.set(ticks);
            return;
        }
        let dt_ms = dt_ms.max(1);
        let pushed_delta = pushed.saturating_sub(self.last_pushed.replace(pushed));
        let shown_delta = shown.saturating_sub(self.last_shown.replace(shown));
        let gen_s = rate_text(pushed_delta, dt_ms);
        let shown_s = rate_text(shown_delta, dt_ms);
        // A pushed frame is either shown or replaced before the UI took it, so
        // this difference is exactly the coalesced count of the same window.
        let coal_s = rate_text(pushed_delta.saturating_sub(shown_delta), dt_ms);
        let (gap_p50, gap_p95, gap_n) = control.present_percentiles();
        window.set_perf_gen(gen_s.clone().into());
        window.set_perf_shown(shown_s.clone().into());
        window.set_perf_coal(coal_s.clone().into());
        window.set_perf_gap(
            if gap_n < 2 {
                "—".to_string()
            } else {
                format!("{} / {} ms", gap_value(gap_p50), gap_value(gap_p95))
            }
            .into(),
        );

        match stats {
            Some(stats) => {
                window.set_perf_decode(format!("{:.1} /s", stats.fps_decoded).into());
                window.set_perf_stalls(format!("{} 次", stats.stalls).into());
                window.set_perf_codec(
                    if stats.width == 0 || stats.height == 0 {
                        crate::codec_name(stats.codecid).to_string()
                    } else {
                        format!(
                            "{} · {}×{} · {:.0} fps",
                            crate::codec_name(stats.codecid),
                            stats.width,
                            stats.height,
                            stats.declared_fps
                        )
                    }
                    .into(),
                );
                // 缓冲: decoded seconds ahead + the ring in frames at the
                // track's own rate. `declared_fps` is the stream's, not a
                // measurement, so it is only the denominator.
                let ring = control.ring_occupancy();
                window.set_perf_net_buffer(
                    format!(
                        "{:.2} s · ring {ring} 帧 / {:.1} fps",
                        stats.buffer_s, stats.declared_fps
                    )
                    .into(),
                );
            }
            None => {
                // No track yet: every video row reads "—" rather than zero.
                window.set_perf_decode("—".into());
                window.set_perf_stalls("—".into());
                window.set_perf_codec("—".into());
                window.set_perf_net_buffer("—".into());
            }
        }

        // 弹幕: the fast lane's tick count. Off means no ticks happen at all,
        // and "—" says so instead of a 0 that would read as a measurement.
        let dm_delta = ticks.saturating_sub(self.last_dm_ticks.replace(ticks));
        window.set_perf_dm_rate(
            if ctx.danmaku_on && ctx.frames_on {
                rate_text(dm_delta, dt_ms)
            } else {
                "—".to_string()
            }
            .into(),
        );
        window.set_perf_dm_paint(
            match self.dm_render_ms.borrow().percentiles() {
                Some((p50, p95)) => format!("{p50:.2} / {p95:.2} ms"),
                None => "—".to_string(),
            }
            .into(),
        );
        window.set_perf_dm_rows(
            if ctx.danmaku_on && ctx.dm_rows > 0 {
                format!("{} 条", ctx.dm_rows)
            } else {
                "—".to_string()
            }
            .into(),
        );

        // UI: the 4 ms probe's own gaps, one paint's cost, and the renderer
        // that actually won at boot (DComp when the player window is the
        // composite one - that is the layer that presents the picture).
        let hwnd = crate::win::hwnd_of(window.window()).unwrap_or(0);
        window.set_perf_ui_gap(
            match self.ui_gap_ms.borrow().percentiles() {
                Some((p50, p95)) => format!("{p50:.2} / {p95:.2} ms"),
                None => "—".to_string(),
            }
            .into(),
        );
        window.set_perf_ui_paint(
            match crate::dcomp::ui_counters(hwnd) {
                Some((_, ms)) if ms > 0.0 => format!("{ms:.2} ms"),
                _ => "—".to_string(),
            }
            .into(),
        );
        let renderer = crate::qa::perf::active_renderer();
        window.set_perf_renderer(
            if crate::dcomp::is_player_window(hwnd) {
                format!("DComp · {renderer}")
            } else {
                renderer.to_string()
            }
            .into(),
        );

        // 网络: counted bytes only. Without the metered proxy there is no
        // rate to report, so the rows read "—" (the old panel's ≈ estimate is
        // gone with the counter that produced it).
        match metered {
            Some(metered) => {
                if metered.bytes_per_sec > self.peak_bps.get() {
                    self.peak_bps.set(metered.bytes_per_sec);
                }
                window.set_perf_net_rate(mbps_text(metered.bytes_per_sec).into());
                window.set_perf_net_peak(mbps_text(self.peak_bps.get()).into());
                window.set_perf_net_conn(
                    format!(
                        "{} / {}",
                        metered.active_workers.min(metered.workers.len() as u32),
                        metered.workers.len()
                    )
                    .into(),
                );
                window.set_perf_net_failed(format!("{} 次", metered.failed_ranges).into());
            }
            None => {
                window.set_perf_net_rate("—".into());
                window.set_perf_net_peak(if self.peak_bps.get() > 0 {
                    mbps_text(self.peak_bps.get()).into()
                } else {
                    "—".into()
                });
                window.set_perf_net_conn("—".into());
                window.set_perf_net_failed("—".into());
            }
        }

        // 插帧: the mode the pacer runs, the tier it resolved, what one
        // generated frame cost, and the rate those frames reached the screen
        // at (the same 上屏 counter as the video group).
        //
        // 档位 is the grid the pacer RESOLVED, never the stored 120 switch: a
        // session whose output ceiling is 60 Hz runs 60 even with the 120
        // setting on, and the row must say what ran (the f201 capture caught it
        // printing 120 next to an `out_hz=60` log line).
        let mode = control.interp_mode();
        let out_hz = control.interp_out_hz();
        window.set_perf_interp_on(mode != 0);
        if mode != 0 {
            window.set_perf_interp_hz(
                if out_hz == 0 {
                    "—".to_string()
                } else {
                    out_hz.to_string()
                }
                .into(),
            );
            // Mode 2 measures its pairs (block matching), mode 3 its optical
            // flow submits. Mode 1 has no pair histogram, so it reports the
            // measured per-frame average (`interp_us / generated`) instead of a
            // percentile it cannot compute.
            let cost = match mode {
                2 => {
                    let (p50, p95) = control.interp_bm_percentiles();
                    (p50 > 0.0 || p95 > 0.0).then(|| format!("{p50:.2} / {p95:.2} ms"))
                }
                3 => {
                    let (p50, p95) = control.interp_ofa_percentiles();
                    (p50 > 0.0 || p95 > 0.0).then(|| format!("{p50:.2} / {p95:.2} ms"))
                }
                _ => {
                    let (generated, _, avg_ms) = control.interp_stats();
                    (generated > 0 && avg_ms > 0.0).then(|| format!("{avg_ms:.2} ms 均"))
                }
            };
            window.set_perf_interp_cost(cost.unwrap_or_else(|| "—".to_string()).into());
            window.set_perf_interp_reach(shown_s.clone().into());
        }

        // The log counterpart of the screen: one line per publish, so a QA run
        // can line the digits up with the player's own `fps_presented` /
        // `shown_s` lines for the same second.
        crate::debug_log(&format!(
            "perf panel t_ms={now_ms} window_ms={dt_ms} pushed={pushed} shown={shown} \
             gen_s={} shown_s={} coal_s={} gap_p50_ms={gap_p50:.2} gap_p95_ms={gap_p95:.2} \
             gap_n={gap_n} dm_ticks={ticks} dm_rows={} dm_paint={} ui_gap={} ui_paint={} \
             renderer={}",
            clean(&gen_s),
            clean(&shown_s),
            clean(&coal_s),
            ctx.dm_rows,
            clean(&window.get_perf_dm_paint().to_string()),
            clean(&window.get_perf_ui_gap().to_string()),
            clean(&window.get_perf_ui_paint().to_string()),
            clean(&window.get_perf_renderer().to_string()),
        ));
    }
}

/// `118.2 /s` for `count` events over `dt_ms`. The window is the publish's own
/// interval, so a short one cannot inflate the rate.
fn rate_text(count: u64, dt_ms: i64) -> String {
    format!("{:.1} /s", count as f64 * 1000.0 / dt_ms.max(1) as f64)
}

/// `42.3 MB/s`, decimal units: the same base as the strip's byte counts, so a
/// rate and a claimed range read off one screen agree.
fn mbps_text(bps: u64) -> String {
    format!("{:.1} MB/s", bps as f64 / 1_000_000.0)
}

/// The pacer's present-interval histogram tops out at 32 ms (128 buckets of
/// 0.25 ms), so a percentile that lands exactly on the cap means "this long or
/// longer". Printing it as an exact `32.00` would be the kind of precision this
/// panel exists to remove, so the cap is spelled out.
const PRESENT_HIST_CAP_MS: f64 = 32.0;

/// One present-interval value, marked with `≥` when it sits on the histogram's
/// last bucket.
fn gap_value(ms: f64) -> String {
    if ms >= PRESENT_HIST_CAP_MS {
        format!("≥{ms:.2}")
    } else {
        format!("{ms:.2}")
    }
}

/// A log field with no spaces in it (the line is parsed by column).
fn clean(text: &str) -> String {
    text.replace(' ', "_")
}

/// The open player's comment state: which video's comments are loaded, the
/// paging cursor, the rows the UI renders, the sort mode and a cache of each
/// comment's dialog (keyed by root `rpid`).
///
/// Held behind `Arc<Mutex<_>>` so the network workers can fill it while the UI
/// thread reads it. The UI publish pass is [`publish_comments`].
#[derive(Default)]
pub(crate) struct CommentsState {
    /// The video whose comments this state describes (`0` before a player opens).
    pub(crate) aid: i64,
    /// `true` = 热门 (mode 3), `false` = 按时间 (mode 2).
    pub(crate) sort_hot: bool,
    /// Cursor for the next page (`""` when the list is exhausted).
    pub(crate) cursor: String,
    pub(crate) is_end: bool,
    pub(crate) total: i64,
    /// The pinned comment, when the video has one.
    pub(crate) top: Option<bilibili::Comment>,
    pub(crate) rows: Vec<bilibili::Comment>,
    /// The page after `cursor`, already fetched. `on_comments_more` appends it
    /// without a network round trip (one page ahead, so scrolling is instant).
    pub(crate) prefetch: Option<CommentsPrefetch>,
    /// A prefetch request is in flight for `cursor`.
    pub(crate) prefetching: bool,
    /// One comment's dialog, cached by root `rpid` (page 1 only).
    pub(crate) replies: HashMap<i64, Vec<bilibili::Comment>>,
    /// `true` while another page of a dialog exists (查看更多回复).
    pub(crate) reply_more: HashMap<i64, bool>,
    /// Root `rpid`s the user expanded, so a republish (load more / avatar
    /// patch / sort change) keeps every open thread open.
    pub(crate) expanded: HashSet<i64>,
    /// The row index -> root `rpid` map of the model the UI currently holds,
    /// written by [`publish_comments`] from the very snapshot it publishes.
    /// The UI names a row by index only (rpids are int64, Slint's `int` is
    /// 32-bit), so this is where a click becomes an id again.
    pub(crate) row_rpids: Vec<i64>,
    /// Raw RGBA avatar pixels by URL. The worker threads fill this so a
    /// republished row keeps the picture it already showed.
    pub(crate) avatars: HashMap<String, (Vec<u8>, u32, u32)>,
    pub(crate) loading: bool,
    pub(crate) error: String,
    /// The window the workers publish to. Weak and `Send`, so it can ride in
    /// this slot; `None` before a player exists (e.g. BILI_COMMENTS_TEST).
    pub(crate) ui: Option<Weak<PlayerWindow>>,
}

/// One page fetched ahead of the panel: the cursor it was requested for (so a
/// sort change or a reload can discard it) and the page itself.
pub(crate) struct CommentsPrefetch {
    pub(crate) aid: i64,
    pub(crate) sort_hot: bool,
    pub(crate) cursor: String,
    pub(crate) page: bilibili::CommentPage,
}

/// Shared handle to the open player's [`CommentsState`].
pub(crate) type CommentsSlot = Arc<Mutex<CommentsState>>;

/// Create and show the player window for `feed_store[index]` and start its
/// playback worker. Replaces the focused player (card_open mode 0).
pub(crate) fn open_player(
    index: i32,
    session_slot: &SessionSlot,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
) {
    open_player_with(
        index,
        OpenMode::Replace,
        session_slot,
        feed_store,
        state,
        settings_store,
    );
}

/// Open `feed_store[index]` in a new window or in place of the focused one.
pub(crate) fn open_player_with(
    index: i32,
    mode: OpenMode,
    session_slot: &SessionSlot,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
) {
    let index = index.max(0) as usize;
    let video = match feed_store.lock() {
        Ok(guard) => guard.get(index).cloned(),
        Err(_) => None,
    };
    let Some(video) = video else {
        crate::debug_log(&format!("card open miss index={index} mode={mode:?}"));
        return;
    };
    let Some(session) = active_session(session_slot) else {
        crate::debug_log(&format!("card open skipped index={index} mode={mode:?} reason=no-session"));
        return;
    };
    open_player_inner(session, video, state, settings_store, mode, None);
}

/// Play one explicit `bvid`/`cid` pair (used by the detail page's 播放 button).
pub(crate) fn open_player_for(
    session_slot: &SessionSlot,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
    bvid: String,
    cid: u64,
    title: String,
    aid: i64,
) {
    let Some(session) = active_session(session_slot) else {
        return;
    };
    let video = bilibili::FeedVideo {
        bvid,
        cid,
        aid: aid.max(0) as u64,
        title,
        pic: String::new(),
        owner: String::new(),
        owner_mid: 0,
        view: 0,
        duration: 0,
        pub_ts: 0,
    };
    open_player_inner(session, video, state, settings_store, OpenMode::Replace, None);
}

/// Open the player for one PGC episode. Playback uses the episode's real
/// `bvid`+`cid` (the verified route), while the panel gets the whole season.
pub(crate) fn open_player_for_pgc(
    session_slot: &SessionSlot,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
    season: bilibili::PgcSeason,
    episode: bilibili::PgcEpisode,
) {
    let Some(session) = active_session(session_slot) else {
        crate::debug_log("pgc player open skipped reason=no-session");
        return;
    };
    crate::debug_log(&format!(
        "pgc player open season={} ep={} bvid={} cid={}",
        season.season_id, episode.ep_id, episode.bvid, episode.cid
    ));
    let video = bilibili::FeedVideo {
        bvid: episode.bvid.clone(),
        cid: episode.cid,
        aid: episode.aid.max(0) as u64,
        title: season.title.clone(),
        pic: season.cover.clone(),
        owner: String::new(),
        owner_mid: 0,
        view: 0,
        duration: episode.duration_secs(),
        pub_ts: 0,
    };
    open_player_inner(
        session,
        video,
        state,
        settings_store,
        OpenMode::Replace,
        Some(PgcPlayer {
            season,
            ep_id: episode.ep_id,
        }),
    );
}

/// Strip sizes; keep in lockstep with `left_panel_w` / `right_panel_w` in
/// `ui/app.slint`.
pub(crate) const PANEL_LEFT_LOGICAL: f64 = 320.0;

pub(crate) const PANEL_RIGHT_LOGICAL: f64 = 360.0;

pub(crate) fn panel_phys(logical: f64, scale: f32) -> i32 {
    (logical * f64::from(scale.max(0.2))).round() as i32
}

/// Physical `(width, height)` a panel set adds to the window. Mirrors the
/// `left_w` / `right_w` bindings in `ui/app.slint`: left/right count when
/// non-zero. The bottom strip was removed with the danmaku-list panel, so the
/// bottom int is inert and its height is always 0.
pub(crate) fn panel_extras(left: i32, right: i32, _bottom: i32, scale: f32) -> (i32, i32) {
    let mut w = 0;
    // The left panel no longer exists (its int is inert); only the right strip
    // can reserve width.
    let _ = left;
    if right != 0 {
        w += panel_phys(PANEL_RIGHT_LOGICAL, scale);
    }
    (w, 0)
}

pub(crate) fn player_window_rect(window: &slint::Window) -> Option<WinRect> {
    let hwnd = win::hwnd_of(window)?;
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd as HWND, &mut rect) == 0 {
            return None;
        }
        Some(WinRect {
            x: rect.left,
            y: rect.top,
            w: rect.right - rect.left,
            h: rect.bottom - rect.top,
        })
    }
}

pub(crate) fn monitor_areas(window: &slint::Window) -> Option<MonitorAreas> {
    let hwnd = win::hwnd_of(window)?;
    unsafe {
        let monitor = MonitorFromWindow(hwnd as HWND, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let to_rect = |r: RECT| WinRect {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        };
        Some(MonitorAreas {
            monitor: to_rect(info.rcMonitor),
            work: to_rect(info.rcWork),
        })
    }
}

/// Move/resize without activating and without touching the z-order, so the
/// player never flickers or steals focus when a panel opens. `SWP_NOCOPYBITS`
/// is deliberately NOT passed: the newly exposed strip must paint.
pub(crate) fn set_player_window_rect(window: &slint::Window, rect: WinRect) -> bool {
    let Some(hwnd) = win::hwnd_of(window) else {
        return false;
    };
    unsafe {
        SetWindowPos(
            hwnd as HWND,
            std::ptr::null_mut(),
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            SWP_NOACTIVATE | SWP_NOZORDER,
        ) != 0
    }
}

pub(crate) fn rect_close(a: WinRect, b: WinRect) -> bool {
    (a.x - b.x).abs() <= 2
        && (a.y - b.y).abs() <= 2
        && (a.w - b.w).abs() <= 2
        && (a.h - b.h).abs() <= 2
}

/// The player's BASE rect in logical px: the window as it would be with no
/// side panel open.
///
/// `[x, y, w, h]`, `x`/`y` being the VIDEO band's top-left - the same `home`
/// the panel code derives - so a restored window grows from the right anchor
/// when a panel opens afterwards. Derived from the live rect minus the strips
/// that are open now, in physical px, then converted with the window's scale
/// factor.
pub(crate) fn player_base_layout(geom: &RefCell<PanelGeom>, window: &slint::Window) -> Option<[i32; 4]> {
    let scale = window.scale_factor().max(0.2);
    let to_logical = f64::from(scale);
    let state = geom.try_borrow().ok()?;
    let base = if state.overlay {
        // Fullscreen: the live rect is the screen, so the remembered
        // pre-fullscreen base is the only meaningful value.
        let (hx, hy) = state.home?;
        [hx, hy, state.base_w.max(1), state.base_h.max(1)]
    } else {
        let live = player_window_rect(window)?;
        let (extra_w, extra_h) = panel_extras(state.left, state.right, state.bottom, scale);
        let left_px = if state.left != 0 {
            panel_phys(PANEL_LEFT_LOGICAL, scale)
        } else {
            0
        };
        [
            live.x.saturating_add(left_px),
            live.y,
            (live.w - extra_w).max(1),
            (live.h - extra_h).max(1),
        ]
    };
    Some([
        (f64::from(base[0]) / to_logical).round() as i32,
        (f64::from(base[1]) / to_logical).round() as i32,
        (f64::from(base[2]) / to_logical).round() as i32,
        (f64::from(base[3]) / to_logical).round() as i32,
    ])
}

/// Persist the base rect, skipping the write (and the log line) when nothing
/// changed. Called on drag end, on a settled move/resize and on both close
/// paths.
pub(crate) fn persist_player_layout(
    geom: &Rc<RefCell<PanelGeom>>,
    window: &slint::Window,
    settings_store: &Arc<Mutex<settings::Settings>>,
    offset: i32,
    tag: &str,
) {
    let Some(mut layout) = player_base_layout(geom, window) else {
        return;
    };
    // A cascaded second window persists the rect the user would get WITHOUT
    // the cascade, so opening/closing extra players never drifts the saved
    // position of the first one.
    layout[0] = layout[0].saturating_sub(offset);
    layout[1] = layout[1].saturating_sub(offset);
    let unchanged = settings_store
        .lock()
        .map(|prefs| prefs.player_layout == Some(layout))
        .unwrap_or(true);
    if unchanged {
        return;
    }
    update_settings(settings_store, |prefs| prefs.player_layout = Some(layout));
    crate::debug_log(&format!(
        "player layout save x={} y={} w={} h={} offset={offset} tag={tag}",
        layout[0], layout[1], layout[2], layout[3]
    ));
}

/// Intersection area (physical px²) of a rect and the work area of the monitor
/// it lands on. `0` when that monitor no longer exists where the rect is.
pub(crate) fn rect_work_overlap(rect: WinRect) -> i32 {
    let Some((wx, wy, ww, wh)) = win::work_area_for_rect(rect.x, rect.y, rect.w, rect.h) else {
        return 0;
    };
    let left = rect.x.max(wx);
    let top = rect.y.max(wy);
    let right = rect.right().min(wx.saturating_add(ww));
    let bottom = rect.bottom().min(wy.saturating_add(wh));
    if right <= left || bottom <= top {
        0
    } else {
        (right - left).saturating_mul(bottom - top)
    }
}

/// Restore the saved player base rect (logical px), BEFORE `show()`.
///
/// The logical position/size are requested from Slint, which applies them when
/// winit creates the window, so the first paint is already at the restored
/// spot. Only a rect that still lands on a monitor's work area is applied;
/// anything else (a hand-edited file, a monitor that is gone) falls back to the
/// OS default placement and is logged as `source=default`.
///
/// Returns the physical target, which [`verify_player_layout`] re-applies and
/// logs once the player has a native handle (the player HWND is created a few
/// frames after `show()`).
pub(crate) fn restore_player_layout(
    player: &PlayerWindow,
    saved: Option<[i32; 4]>,
    offset: i32,
) -> Option<WinRect> {
    let window = player.window();
    let rect = saved?;
    let scale = f64::from(window.scale_factor().max(0.2));
    // Clamp to the UI's own minimum: a hand-edited file must not ask for a
    // window the player layout cannot fit.
    let w = rect[2].max(PLAYER_MIN_VIDEO_W as i32);
    let h = rect[3].max(PLAYER_MIN_VIDEO_H as i32);
    // A NEW window opened next to existing ones is cascaded by `offset`
    // logical px, so it is visibly a second player instead of hiding exactly
    // behind the first.
    let x = rect[0].saturating_add(offset);
    let y = rect[1].saturating_add(offset);
    let target = WinRect {
        x: (f64::from(x) * scale).round() as i32,
        y: (f64::from(y) * scale).round() as i32,
        w: (f64::from(w) * scale).round() as i32,
        h: (f64::from(h) * scale).round() as i32,
    };
    if rect_work_overlap(target) == 0 {
        crate::debug_log(&format!(
            "player layout restore x={x} y={y} w={w} h={h} source=default reason=offscreen"
        ));
        return None;
    }
    window.set_position(slint::LogicalPosition::new(x as f32, y as f32));
    window.set_size(slint::LogicalSize::new(w as f32, h as f32));
    Some(target)
}

/// Apply and then LOG the player layout once the window has a native handle.
///
/// Retries on a 16ms timer (the player's HWND is mapped a few frames after
/// `show()`), re-applies the physical target so a stale pre-show scale factor
/// cannot misplace the window, and emits the one
/// `player layout restore ... source=saved|default` line that proves which
/// branch ran.
pub(crate) fn verify_player_layout(weak: Weak<PlayerWindow>, target: Option<WinRect>, tries: u32) {
    slint::Timer::single_shot(Duration::from_millis(16), move || {
        let Some(player) = weak.upgrade() else {
            return;
        };
        let window = player.window();
        if win::hwnd_of(window).is_none() {
            if tries > 0 {
                verify_player_layout(weak.clone(), target, tries - 1);
            } else {
                crate::debug_log("player layout restore pending no-hwnd");
            }
            return;
        }
        let mut source = "default";
        if let Some(target) = target {
            if rect_work_overlap(target) > 0 && set_player_window_rect(window, target) {
                source = "saved";
            }
        }
        let scale = f64::from(window.scale_factor().max(0.2));
        let applied = player_window_rect(window).map(|rect| {
            [
                (f64::from(rect.x) / scale).round() as i32,
                (f64::from(rect.y) / scale).round() as i32,
                (f64::from(rect.w) / scale).round() as i32,
                (f64::from(rect.h) / scale).round() as i32,
            ]
        });
        let [x, y, w, h] = applied.unwrap_or([0, 0, 0, 0]);
        crate::debug_log(&format!(
            "player layout restore x={x} y={y} w={w} h={h} source={source}"
        ));
    });
}

/// Native minimum size: the smallest video band plus the strips that are open,
/// so the player can always be shrunk down to a valid picture.
pub(crate) fn player_min_track(window: &slint::Window, left: i32, right: i32, bottom: i32) {
    let scale = window.scale_factor().max(0.2);
    let (extra_w, extra_h) = panel_extras(left, right, bottom, scale);
    let w = panel_phys(PLAYER_MIN_VIDEO_W, scale) + extra_w;
    let h = panel_phys(PLAYER_MIN_VIDEO_H, scale) + extra_h;
    win::set_min_track_size(window, w.max(1), h.max(1));
}

/// `win::set_min_track_size` records ONE window process-wide, so the player's
/// minimum replaces the main window's while it is up. Both close paths hand
/// the slot back (values kept in lockstep with the main window's call).
pub(crate) fn restore_main_min_track(state: &Rc<PlayerState>) {
    if let Some(main) = state.main.upgrade() {
        let scale = main.window().scale_factor();
        win::set_min_track_size(
            main.window(),
            (PLAYER_MIN_VIDEO_W * f64::from(scale)) as i32,
            (360.0 * f64::from(scale)) as i32,
        );
    }
}

/// Grow/shrink the player window to the given panel ints.
///
/// `force` re-applies the CURRENT ints (used when returning from fullscreen)
/// and trusts the base/home captured on fullscreen entry instead of
/// re-deriving them from the rect winit just restored.
pub(crate) fn apply_panel_geometry(
    geom: &Rc<RefCell<PanelGeom>>,
    window: &slint::Window,
    left: i32,
    right: i32,
    bottom: i32,
    force: bool,
) {
    let mut state = geom.borrow_mut();
    if !force && state.left == left && state.right == right && state.bottom == bottom {
        return;
    }
    let scale = window.scale_factor().max(0.2);
    let old_left = state.left;
    let (old_extra_w, old_extra_h) = panel_extras(state.left, state.right, state.bottom, scale);
    let Some(live) = player_window_rect(window) else {
        state.left = left;
        state.right = right;
        state.bottom = bottom;
        crate::debug_log(&format!(
            "panel left={left} right={right} bottom={bottom} overlay={} win=? base={}x{} no-hwnd",
            state.overlay, state.base_w, state.base_h
        ));
        return;
    };
    // Fullscreen: the strips float over the picture, the window is fullscreen
    // already and must not be resized. Remember the ints for the way out.
    if state.overlay {
        state.left = left;
        state.right = right;
        state.bottom = bottom;
        state.applied = None;
        crate::debug_log(&format!(
            "panel left={left} right={right} bottom={bottom} overlay=true win={}x{} \
             base={}x{} fullscreen-skip",
            live.w, live.h, state.base_w, state.base_h
        ));
        return;
    }
    let old_left_px = if old_left != 0 {
        panel_phys(PANEL_LEFT_LOGICAL, scale)
    } else {
        0
    };
    let live_video = (live.x.saturating_add(old_left_px), live.y);
    if force {
        // Returning from fullscreen: base/home were captured before the
        // flip, so only trust the live rect when they were never captured.
        if state.base_w <= 0 || state.base_h <= 0 || state.home.is_none() {
            state.base_w = (live.w - old_extra_w).max(1);
            state.base_h = (live.h - old_extra_h).max(1);
            state.home = Some(live_video);
        }
    } else {
        // Derive the no-panel base from the live size at every change: a
        // manual resize while a panel is open becomes part of the base.
        state.base_w = (live.w - old_extra_w).max(1);
        state.base_h = (live.h - old_extra_h).max(1);
        match state.applied {
            Some(applied) if rect_close(applied, live) => {}
            Some(applied) => {
                // The user moved the window while a panel was open: the
                // remembered home follows by the same delta.
                if let Some((hx, hy)) = state.home {
                    state.home = Some((
                        hx.saturating_add(live.x - applied.x),
                        hy.saturating_add(live.y - applied.y),
                    ));
                }
            }
            None => state.home = Some(live_video),
        }
    }
    let (home_x, home_y) = state.home.unwrap_or(live_video);
    let (new_extra_w, new_extra_h) = panel_extras(left, right, bottom, scale);
    let new_left_px = if left != 0 {
        panel_phys(PANEL_LEFT_LOGICAL, scale)
    } else {
        0
    };
    let mut target = WinRect {
        x: home_x.saturating_sub(new_left_px),
        y: home_y,
        w: (state.base_w + new_extra_w).max(1),
        h: (state.base_h + new_extra_h).max(1),
    };
    // Clamp into the work area: a right/bottom overflow slides the window
    // left/up by exactly that overflow. `home` keeps the unclamped spot, so
    // closing the panel restores the original rect. A window the user parked
    // partly off-screen is left alone unless it actually overflows.
    if let Some(area) = monitor_areas(window) {
        let work = area.work;
        if target.right() > work.right() {
            target.x = target.x.saturating_sub(target.right() - work.right());
            if target.x < work.x {
                target.x = work.x;
            }
        }
        if target.bottom() > work.bottom() {
            target.y = target.y.saturating_sub(target.bottom() - work.bottom());
            if target.y < work.y {
                target.y = work.y;
            }
        }
    }
    let ok = set_player_window_rect(window, target);
    state.applied = Some(target);
    state.left = left;
    state.right = right;
    state.bottom = bottom;
    player_min_track(window, left, right, bottom);
    crate::debug_log(&format!(
        "panel left={left} right={right} bottom={bottom} overlay=false win={}x{} base={}x{} \
         rect={} video={}x{} ok={ok} scale={scale:.2}",
        target.w,
        target.h,
        state.base_w,
        state.base_h,
        target,
        target.w - new_extra_w,
        target.h - new_extra_h
    ));
}

/// Capture the pre-fullscreen base size and the video's ideal position before
/// the window goes fullscreen; panels then float over the picture untouched.
pub(crate) fn enter_panel_overlay(geom: &Rc<RefCell<PanelGeom>>, window: &slint::Window) {
    let mut state = geom.borrow_mut();
    if state.overlay {
        return;
    }
    let scale = window.scale_factor().max(0.2);
    if let Some(live) = player_window_rect(window) {
        let (extra_w, extra_h) = panel_extras(state.left, state.right, state.bottom, scale);
        state.base_w = (live.w - extra_w).max(1);
        state.base_h = (live.h - extra_h).max(1);
        let left_px = if state.left != 0 {
            panel_phys(PANEL_LEFT_LOGICAL, scale)
        } else {
            0
        };
        state.home = Some((live.x.saturating_add(left_px), live.y));
    }
    state.overlay = true;
    state.applied = None;
    crate::debug_log(&format!(
        "panel overlay=true enter left={} right={} bottom={} base={}x{}",
        state.left, state.right, state.bottom, state.base_w, state.base_h
    ));
}

/// Returning from fullscreen: the window is back at its pre-fullscreen rect;
/// grow it by whatever strips are open now.
pub(crate) fn exit_panel_overlay(geom: &Rc<RefCell<PanelGeom>>, window: &slint::Window) {
    let (left, right, bottom) = {
        let mut state = geom.borrow_mut();
        state.overlay = false;
        state.applied = None;
        (state.left, state.right, state.bottom)
    };
    apply_panel_geometry(geom, window, left, right, bottom, true);
}

/// Winit restores the pre-fullscreen rect asynchronously; poll until the window
/// no longer covers the whole monitor, then regrow.
pub(crate) fn schedule_overlay_exit(
    geom: Rc<RefCell<PanelGeom>>,
    weak: Weak<PlayerWindow>,
    tries: u32,
) {
    slint::Timer::single_shot(Duration::from_millis(120), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let still_fullscreen = match (
            player_window_rect(window.window()),
            monitor_areas(window.window()),
        ) {
            (Some(live), Some(area)) => {
                live.w >= area.monitor.w && live.h >= area.monitor.h
            }
            _ => false,
        };
        if still_fullscreen && tries < 25 {
            schedule_overlay_exit(geom.clone(), weak.clone(), tries + 1);
            return;
        }
        exit_panel_overlay(&geom, window.window());
    });
}

/// Build and drive the player window for a resolved video.
///
/// `mode` decides what happens to the players already up: [`OpenMode::Replace`]
/// closes the focused one first (card_open mode 0 / queue / detail 播放), while
/// [`OpenMode::NewWindow`] leaves them untouched and adds a cascaded window
/// (card_open mode 1).
/// One folder row of the player's 收藏 picker (f179). `confirmed` is false when
/// the folder list did not report this video's membership: the UI then says
/// 未确认 instead of drawing a check mark nobody verified.
#[derive(Clone)]
pub(crate) struct FavPick {
    pub(crate) fid: i64,
    pub(crate) title: String,
    pub(crate) count: u32,
    pub(crate) checked: bool,
    pub(crate) confirmed: bool,
}

impl FavPick {
    /// `收藏夹/资源` rows for the Slint model.
    fn rows(rows: &[FavPick]) -> Vec<PlayerFavRow> {
        rows.iter()
            .map(|row| PlayerFavRow {
                fid: row.fid.to_string().into(),
                title: row.title.clone().into(),
                count: row.count.min(i32::MAX as u32) as i32,
                checked: row.checked,
                confirmed: row.confirmed,
            })
            .collect()
    }
}

/// Publish the picker: the rows, the 收藏 chip's state (solid when ANY folder
/// holds the video) and one optional line of feedback. Also mirrors the rows
/// into the shared slot so the next toggle starts from what the user sees.
fn publish_fav_picker(
    window: &PlayerWindow,
    slot: &Arc<Mutex<Vec<FavPick>>>,
    rows: Vec<FavPick>,
    hint: Option<(&str, bool)>,
) {
    if let Ok(mut guard) = slot.lock() {
        *guard = rows.clone();
    }
    let any = rows.iter().any(|row| row.checked);
    window.set_fav_items(ModelRc::from(std::rc::Rc::new(VecModel::from(FavPick::rows(
        &rows,
    )))));
    window.set_fav_loading(false);
    window.set_favorited(any);
    window.set_fav_text(if any { "已收藏" } else { "收藏" }.into());
    if let Some((text, error)) = hint {
        window.set_fav_hint(text.into());
        window.set_fav_hint_error(error);
    }
}

/// f202: publish 画质增强 — the tier in effect, which tiers this machine can
/// run, and the one line that explains the greyed ones.
///
/// The tiers only exist in the DComp picture path (`src/dcomp/video.rs`); on the
/// winit path there is no video layer to draw into, so 1–3 are greyed with that
/// reason rather than silently doing nothing.
///
/// f204: the same publish also fills the 画面直出 row: `player_dcomp` is the
/// stored preference the row edits, `dcomp_active` is the path this process
/// actually runs on (`platform::enabled()` was resolved at start-up). The two
/// differ exactly when the user flipped the switch and has not restarted yet,
/// which is what the row's "重启后生效" wording is derived from.
fn publish_enhance(window: &PlayerWindow, player_dcomp: bool) {
    let dcomp = crate::dcomp::platform::enabled();
    let availability = crate::dcomp::enhance::availability();
    let shader_ok = dcomp && availability.shaders;
    let mut parts: Vec<String> = Vec::new();
    if !dcomp {
        parts.push("未开启画面直出：锐化 / Anime4K / FSR（1–3 档）不可用".to_string());
    } else if !availability.shaders {
        parts.push(format!("锐化 / Anime4K / FSR：{}", availability.shader_reason));
    }
    window.set_enhance_shader_ok(shader_ok);
    window.set_enhance_hint(parts.join("；").into());
    window.set_player_dcomp(player_dcomp);
    window.set_dcomp_active(dcomp);
    // A stored tier this machine cannot run must not look selected. Falling
    // back here (once, before the first frame) is what keeps a settings.json
    // copied from another machine from reporting a filter that is not running.
    let wanted = crate::dcomp::enhance::desired();
    let allowed = wanted == crate::dcomp::enhance::OFF || shader_ok;
    if !allowed {
        crate::dcomp::enhance::set_desired(crate::dcomp::enhance::OFF);
        window.set_enhance_mode(crate::dcomp::enhance::OFF as i32);
    }
    crate::debug_log(&format!(
        "enhance publish mode={} dcomp={dcomp} player_dcomp={player_dcomp} shaders={} hint={}",
        crate::dcomp::enhance::desired(),
        availability.shaders,
        parts.join("；")
    ));
}

pub(crate) fn open_player_inner(
    session: Arc<bilibili::Session>,
    video: bilibili::FeedVideo,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
    mode: OpenMode,
    pgc: Option<PgcPlayer>,
) {
    let bvid = video.bvid.clone();
    let aid = video.aid as i64;
    let cid = video.cid;

    // Anchor this player's comment state to its own video. The slot is created
    // per player (NOT on the shared `PlayerState`): several player windows can be
    // open at once, and a shared slot let the newest player take over the older
    // one's comment panel. The aid comes from the feed path (`FeedVideo.aid`) or
    // the card path (`DetailState.aid` via `open_player_for`); it is logged so a
    // QA run can see which one it was.
    let comments: CommentsSlot = Arc::new(Mutex::new(CommentsState {
        aid,
        sort_hot: true,
        ..CommentsState::default()
    }));
    crate::debug_log(&format!("player comments aid={aid} bvid={bvid}"));
    // The four callbacks (`comments_load` / `comments_more` / `comments_sort` /
    // `comment_replies`) plus `comment_toggle` are registered further down, once
    // the window exists; see the "Player comments" section.

    // Replace closes exactly the focused window through its own close path
    // (loops, control, queue hand-off). NewWindow touches nothing.
    let replacing = if mode == OpenMode::Replace {
        focused_session(state)
    } else {
        None
    };
    if let Some(old) = replacing.as_ref() {
        let close_started = std::time::Instant::now();
        crate::debug_log(&format!(
            "card open replace closed_win={} bvid={bvid}",
            old.id
        ));
        close_player_window(
            state,
            &old.window.as_weak(),
            &Rc::downgrade(old),
            settings_store,
            "replace",
        );
        crate::debug_log(&format!(
            "card open replace close_ms={}",
            close_started.elapsed().as_millis()
        ));
    }
    let open_count = state.players.borrow().len();
    crate::debug_log(&format!(
        "player open mode={mode:?} bvid={bvid} existing={open_count}"
    ));

    let player = match crate::dcomp::with_player_scope(PlayerWindow::new) {
        Ok(player) => player,
        Err(_) => return,
    };
    let title_for_mini = video.title.clone();
    player.global::<AppTheme>().set_dark(state.theme_dark.get());
    // f194: the player window built by the DComp adapter renders its Slint UI
    // with the software renderer. Tell the theme so the danmaku layer can drop
    // the second Text per item (see `soft-ui` in `ui/app.slint`): that is the
    // difference between ~50 and ~70 UI-layer updates per second while the
    // danmaku move.
    if let Some(hwnd) = win::hwnd_of(player.window()) {
        player
            .global::<AppTheme>()
            .set_soft_ui(crate::dcomp::is_player_window(hwnd));
    }
    player.set_video_title(video.title.clone().into());
    // Left sidebar's video-info header: everything the feed row already knows
    // (the card/detail path fills what it has and leaves the rest empty).
    player.set_info_title(video.title.clone().into());
    player.set_info_owner(
        if video.owner.is_empty() {
            "UP主未知".to_string()
        } else {
            format!("UP主 · {}", video.owner)
        }
        .into(),
    );
    let stats = match (video.view, video.duration) {
        (0, 0) => String::new(),
        (view, 0) => format!("{} 次播放", format_view(view)),
        (0, duration) => format_duration(duration),
        (view, duration) => format!("{} 次播放 · {}", format_view(view), format_duration(duration)),
    };
    player.set_info_stats(stats.into());
    // 番剧 fills the SAME info tab from the season detail (score / 类型 / 首播 /
    // 状态 / 简介) instead of inventing a second panel. `info_desc` is the
    // collapsible 简介 row added for this case; every other video leaves it
    // empty, so the tab is unchanged for archives.
    if let Some(pgc) = pgc.as_ref() {
        let season = &pgc.season;
        player.set_info_title(season.title.clone().into());
        let classify = season.classify();
        player.set_info_owner(
            if classify.is_empty() {
                "番剧".to_string()
            } else {
                format!("番剧 · {classify}")
            }
            .into(),
        );
        let mut stats: Vec<String> = Vec::new();
        if season.score > 0.0 {
            if season.score_count > 0 {
                stats.push(format!("评分 {:.1}（{}人）", season.score, season.score_count));
            } else {
                stats.push(format!("评分 {:.1}", season.score));
            }
        }
        if !season.index_show.trim().is_empty() {
            stats.push(season.index_show.clone());
        }
        if !season.pub_time.trim().is_empty() {
            stats.push(format!("首播 {}", season.pub_time));
        }
        stats.push(if season.is_finish {
            "已完结".to_string()
        } else {
            "连载中".to_string()
        });
        player.set_info_stats(stats.join(" · ").into());
        let evaluate = season.evaluate.trim();
        player.set_info_desc(evaluate.into());
    }
    // Transient messages are the toast stack now; `player_status` /
    // `interaction_status` stay declared but are never written (the UI keeps
    // them inert).
    player.set_playing(true);
    player.set_position_ms(0.0);
    player.set_duration_ms(0.0);
    // The very first frame the window paints must be the buffering layer: the
    // pre-roll gate has not armed yet (playurl + decoder spawn take a moment)
    // and the tick would otherwise publish `buffering=false` over an empty
    // picture. Set synchronously, before `show()`, so no frame can leak the
    // bare tint; `on_danmaku_tick` keeps both values in sync after.
    player.set_buffering(true);
    player.set_buffer_progress(0.0);
    player.set_time_current(format_mmss(0).into());
    player.set_time_total(format_mmss(0).into());
    player.set_time_remaining(format!("-{}", format_mmss(0)).into());
    player.set_volume(100.0);
    player.set_volume_text("100".into());
    player.set_muted(false);
    player.set_speed_milli(1000.0);
    player.set_speed_text("1.0x".into());
    player.set_danmaku_on(true);
    player.set_danmaku_text("弹幕开".into());
    player.set_fullscreen(false);
    player.set_fullscreen_text("全屏".into());
    player.set_quality_qn(0);
    player.set_quality_label("画质".into());
    player.set_liked(false);
    player.set_like_text("点赞".into());
    player.set_coin_multiply(0);
    player.set_coin_text("投币".into());
    player.set_favorited(false);
    player.set_fav_text("收藏".into());
    player.set_topmost(false);
    player.set_topmost_text("置顶".into());
    player.set_mini_open(false);
    player.set_audio_index(0);
    player.set_audio_label("音轨".into());
    // 0 = 适配 (contain): letterbox, never stretch. Must match the Slint
    // property default in `ui/app.slint`; this explicit seed would otherwise
    // override it at open and resize would distort the picture.
    player.set_fit_mode(0);
    player.set_fit_menu_open(false);
    player.set_brightness(0);
    player.set_brightness_menu_open(false);
    player.set_audio_only(false);
    // `info_open` / `comments_open` / `panel_open` are inert since the player
    // redesign: the three `*_panel` ints below own the layout and the UI's own
    // toggle functions write them. Nothing here drives the old flags (their
    // defaults are false), so the two systems cannot fight.
    player.set_info_lines(ModelRc::from(std::rc::Rc::new(VecModel::from(
        Vec::<SharedString>::new(),
    ))));
    player.set_sleep_text(String::new().into());
    player.set_sleep_menu_open(false);
    player.set_history_dates(ModelRc::from(std::rc::Rc::new(VecModel::from(
        Vec::<SharedString>::new(),
    ))));
    player.set_history_status("点击「历史」获取可用日期".into());
    player.set_history_selected(String::new().into());
    player.set_send_status(String::new().into());
    player.set_danmaku_input(
        std::env::var("BILI_DANMAKU_SEED")
            .unwrap_or_default()
            .into(),
    );

    let prefs = settings_store
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    // Codec preference, captured for the 画质 row republishes below (a plain
    // copy, so the closures never hold the settings lock).
    let codec_pref = prefs.codec_pref as i64;
    // The player has its own `AppTheme` instance: mirror the stored transparency
    // so its tint matches the main window.
    player
        .global::<AppTheme>()
        .set_glass_transparency(prefs.glass_transparency);
    player.set_danmaku_opacity(prefs.danmaku_opacity as f32 / 100.0);
    player.set_danmaku_opacity_text(prefs.danmaku_opacity.to_string().into());
    player.set_danmaku_font(prefs.danmaku_font as i32);
    player.set_danmaku_speed(prefs.danmaku_speed as i32);
    player.set_danmaku_area(prefs.danmaku_area as i32);
    player.set_block_scroll(prefs.block_scroll);
    player.set_block_top(prefs.block_top);
    player.set_block_bottom(prefs.block_bottom);
    player.set_speed_milli(prefs.player_speed as f32);
    player.set_speed_text(format_speed(prefs.player_speed as f64 / 1000.0).into());
    player.set_key_map(key_map_model(&prefs.player_keys));
    // f202: restore 画质增强 before the first frame is presented, so a stored
    // Anime4K/FSR tier is already the one the video thread reads.
    crate::dcomp::enhance::set_desired(prefs.player_enhance);
    player.set_enhance_mode(prefs.player_enhance as i32);

    // Frameless at the moment of presentation: strip the caption and install
    // the non-client subclass synchronously, before/around `show()`, so the
    // player is never visible decorated. If the handle is not ready before
    // `show()`, repeat on the very next statement (no timer, no event round trip).
    let player_prepared = win::prepare_frameless(player.window());
    // Restore the last window position/size BEFORE showing: the first painted
    // frame then already sits where it will stay, and the seeded backdrop can be
    // placed from that target without waiting for the HWND.
    let saved_layout = settings_store
        .lock()
        .map(|prefs| prefs.player_layout)
        .unwrap_or(None);
    // A second window is cascaded so it never lands exactly on the first.
    let cascade = if mode == OpenMode::NewWindow {
        36 * open_count as i32
    } else {
        0
    };
    let layout_target = restore_player_layout(&player, saved_layout, cascade);
    // The backdrop is generated, so it is already in place before `show()`: the
    // first painted frame is the gradient + tint, never a transparent window.
    set_player_backdrop(&player, state.theme_dark.get());
    if player.show().is_err() {
        return;
    }
    if !player_prepared {
        win::prepare_frameless(player.window());
    }
    // The wallpaper is anchored to the SCREEN position, so publish it again now
    // that the native window is placed (the pre-show publish had no rect).
    let (wall_x, wall_y) = publish_wallpaper(player.global::<AppTheme>(), player.window());
    crate::debug_log(&format!(
        "wallpaper origin win=player stage=show x={wall_x:.1} y={wall_y:.1}"
    ));
    crate::debug_log(&format!(
        "player prepare pre-show={player_prepared} backdrop=1"
    ));
    // Smallest size the player can be dragged to: the video band's minimum
    // plus the strips that are open (none yet at this point).
    player_min_track(player.window(), 0, 0, 0);
    verify_player_layout(player.as_weak(), layout_target, 120);

    // The player is up: suspend every main-window background job that would
    // otherwise compete with playback. Cover fetches in flight finish quietly
    // (their patch is dropped in `stream_thumbs`), no new fetch starts and the
    // skeleton shimmer stops. Every close path decrements the counter, and the
    // main window resumes only at zero.
    player_opened();
    if let Some(main) = state.main.upgrade() {
        skeleton_halt(&main);
    }

    // Frameless: the subclass removes the non-client area entirely (no caption,
    // no native frame flash) while keeping WS_THICKFRAME for resize/snap. The
    // HWND is mapped a few frames after show(), so retry on the UI-thread timer.
    retry_subclass(&state);

    let weak = player.as_weak();
    // The comment workers publish through this weak handle (see `publish_comments`);
    // the aid/sort fields were anchored to this video above.
    if let Ok(mut guard) = comments.lock() {
        guard.ui = Some(weak.clone());
    }
    // 外扩面板 geometry, one per player window: the base size and the last
    // applied rect are per-window state, captured by this player's own
    // `panel_changed` handler below, by its danmaku tick and by the drag-end
    // layout persist.
    let panel_geom: Rc<RefCell<PanelGeom>> = Rc::new(RefCell::new(PanelGeom::default()));
    let player_drag = Rc::new(win::WindowDrag::new());
    // Same frame-locked move as the main window: the backdrop is a generated
    // image, so a move needs no re-placement at all.
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
                crate::debug_log(&format!("drag denied reason={reason} source=player"));
                return;
            }
            stats.begin();
            crate::debug_log(&format!(
                "drag start ok=1 source={} pos={}",
                drag_source(&player.window(), "player"),
                drag_pos(&player.window()),
            ));
            if timer.running() {
                return;
            }
            let tick_weak = weak.clone();
            let tick_drag = drag.clone();
            let tick_timer = Rc::downgrade(&timer);
            let tick_stats = stats.clone();
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
                    let Some(timer) = tick_timer.upgrade() else {
                        return;
                    };
                    let Some(player) = tick_weak.upgrade() else {
                        timer.stop();
                        return;
                    };
                    // See the main-window tick: the real VK_LBUTTON release is
                    // the only thing that ends a drag, so a grab Slint dropped
                    // while the window moved under the cursor cannot strand
                    // the drag half-way (the user had to press again).
                    if !tick_drag.is_dragging() {
                        timer.stop();
                        return;
                    }
                    if win::is_visible(&player.window()) == Some(false) {
                        crate::debug_log("drag end reason=window-left source=player");
                        tick_stats.finish("player");
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
                            "drag end reason=pointer-up deferred=0 moved={moved} source=player"
                        ));
                        tick_stats.finish("player");
                        tick_drag.end();
                        timer.stop();
                    }
                },
            );
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
        let cascade = cascade;
        let stats = player_drag_stats.clone();
        player.on_window_drag_end(move || {
            if let Some(player) = weak.upgrade() {
                if !drag.end_on_signal(win::left_button_down()) {
                    crate::debug_log(&format!(
                        "drag end reason=lost-grab deferred=1 source=player {}",
                        drag_state(&player.window())
                    ));
                    return;
                }
                timer.stop();
                if drag.flush(&player.window()) {
                    stats.placement();
                }
                // Drag-end persist (task 4): the settled base rect.
                persist_player_layout(&geom, player.window(), &store, cascade, "drag-end");
            }
            timer.stop();
            drag.end();
            stats.finish("player");
            crate::debug_log("drag end reason=pointer-up deferred=0 source=player");
        });
    }
    // Diagnostic hook (`BILI_PLAYER_DRAG_TEST=<ms>`): drag this player window
    // while its video plays and log the achieved placements/s. Started well
    // after the window is up so the drag lands inside playback, and driven by
    // the same `WindowDrag` calls the UI makes (no synthetic input).
    if let Ok(value) = std::env::var("BILI_PLAYER_DRAG_TEST") {
        let duration = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(2000)
            .clamp(200, 30_000);
        run_player_drag_rate_test(
            weak.clone(),
            player_drag.clone(),
            player_drag_stats.clone(),
            Duration::from_millis(6000),
            Duration::from_millis(duration),
            240,
        );
    }
    // ---- self-driven window resize (same contract as the main window) ----
    // The player's 400ms resize-restart is driven by the display-size watch in
    // `start_player_loops`, which reads the native rect at 60Hz: it therefore
    // fires once the gestured rect settles, exactly as with a native resize.
    let player_resize = Rc::new(win::WindowResize::new());
    player_resize.set_min(524, 270);
    let player_resize_timer = Rc::new(slint::Timer::default());
    {
        let weak = weak.clone();
        let resize = player_resize.clone();
        let timer = player_resize_timer.clone();
        player.on_resize_start(move |edge: i32| {
            let Some(player) = weak.upgrade() else {
                return;
            };
            // Fullscreen and maximised windows have no meaningful anchored
            // edge (the picture must stay with the monitor).
            if player.window().is_fullscreen() || player.window().is_maximized() {
                return;
            }
            if let Err(reason) = resize.start(&player.window(), edge) {
                crate::debug_log(&format!(
                    "resize denied reason={reason} edge={edge} source=player"
                ));
                return;
            }
            crate::debug_log(&format!(
                "resize start edge={edge} source=player rect={}",
                resize_rect_text(&player.window())
            ));
            if timer.running() {
                return;
            }
            let tick_weak = weak.clone();
            let tick_resize = resize.clone();
            let tick_timer = Rc::downgrade(&timer);
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
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
                        crate::debug_log("resize end reason=window-left source=player");
                        tick_resize.end();
                        timer.stop();
                        return;
                    }
                    tick_resize.update(&player.window());
                    if !win::left_button_down() {
                        crate::debug_log("resize end reason=pointer-up source=player");
                        tick_resize.end();
                        timer.stop();
                    }
                },
            );
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
                crate::debug_log("resize end reason=lost-grab deferred=1 source=player");
                return;
            }
            if let Some(player) = weak.upgrade() {
                // Apply the last pending cursor position, then persist the
                // settled base rect. The debounce (tag=settle) writes it too;
                // this release-time write keeps the exact size even when the
                // app closes inside the 600ms settle window.
                resize.update(&player.window());
                persist_player_layout(&geom, player.window(), &store, cascade, "resize-end");
            }
            timer.stop();
            resize.end();
        });
    }
    let control = player::PlaybackControl::new();
    // f212: publish the window handle before any decode thread can ask for the
    // video layer. The DComp side already knows this window (its composition
    // tree is attached by now), so the zero-copy decision no longer needs the
    // event loop — see `player::start_mf_session`.
    control.set_window_hwnd(win::hwnd_of(player.window()).unwrap_or(0));
    // The video this window starts on: a 选集 pick replaces it in place (see
    // `PlaybackControl::switch_to`), so the control has to know it from the
    // first click, not only once the decode thread has armed.
    control.set_source(&bvid, cid, aid, &video.title);
    // 音质 request: the persisted DASH audio id (0 = 自动（最高）). The env
    // override keeps the flow verifiable without touching a stored setting.
    let audio_quality_request: i64 = std::env::var("BILI_AUDIO_QUALITY")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(i64::from(prefs.audio_quality));
    let _ = control.set_audio_quality(audio_quality_request);
    player.set_audio_quality(audio_quality_request as i32);
    control.set_danmaku_font(prefs.danmaku_font);
    control.set_danmaku_speed(prefs.danmaku_speed);
    control.set_danmaku_area(prefs.danmaku_area);
    control.set_type_blocks(prefs.block_scroll, prefs.block_top, prefs.block_bottom);
    control.set_blocked_senders(prefs.blocked_senders.clone());
    control.set_block_rules(&prefs.blocked_keywords, &prefs.blocked_regex);
    control.set_hwaccel(prefs.hwaccel);
    control.set_codec_pref(prefs.codec_pref as i64);
    control.set_speed(prefs.player_speed as f64 / 1000.0);
    // 帧插值: the persisted mode applies to this window from the first frame.
    // `set_interp_mode` normalises it (0 = 关闭, 1 = 混合, 2 = 块匹配, 3 = OFA,
    // anything else = off) and is what the badge and the pacer read.
    control.set_interp_mode(prefs.interp_mode);
    // 插帧帧率: persisted 60/120. The pacer reads it when it resolves the next
    // session's grid; 120 additionally needs the picture surface (see
    // `player::video_surface`) and the self-check below.
    control.set_interp_120(prefs.interp_120);
    start_gpu_sampler();
    // The three interpolation labels are republished by the tick on change; seed
    // them now so the row never shows a stale/blank mode for the first frames.
    player.set_interp_mode_text(control.interp_mode_text().into());
    player.set_interp_mode(i32::from(control.interp_mode()));
    player.set_interp_120(control.interp_120());
    player.set_interp_120_ok(true);
    player.set_interp_stats_text(control.interp_stats_text().into());
    player.set_interp_gpu_text(gpu_utilization_text().into());
    // `interp_cycle()` (the row and the shortcut menu item) advances
    // 关闭 -> 混合 -> 块匹配 -> OFA -> 关闭, applies it to this player and
    // persists it. The OFA step is skipped while the runtime is missing, because
    // a machine without `nvofapi64.dll` can never run it: the click would only
    // land on a mode that blends anyway. Only the cheap runtime check is used
    // here (no device is created on the UI thread); a machine whose engine still
    // fails gets one log line and one toast from the pacer instead.
    {
        let control = control.clone();
        let window = weak.clone();
        let store = settings_store.clone();
        let ofa_skip_logged = Rc::new(Cell::new(false));
        player.on_interp_cycle(move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            let mut next = (control.interp_mode() + 1) % 4;
            if next == player::INTERP_MODE_OFA && !player::ofa_present() {
                if !ofa_skip_logged.replace(true) {
                    crate::debug_log("interp cycle skip mode=3 reason=dll_missing");
                }
                next = 0;
            }
            control.set_interp_mode(next);
            update_settings(&store, |prefs| prefs.interp_mode = next);
            window.set_interp_mode_text(control.interp_mode_text().into());
            // Same turn as the state change: the badge's int can never lag the
            // control by a tick.
            window.set_interp_mode(i32::from(control.interp_mode()));
            window.set_interp_stats_text(control.interp_stats_text().into());
            crate::debug_log(&format!(
                "interp cycle mode={next} text={} source=ui",
                control.interp_mode_text()
            ));
        });
    }
    // f190 平滑 menu: a row picks an ABSOLUTE mode, so 混合 / 块匹配 / OFA is one
    // press away instead of one-to-three cycle steps. Same tail as the cycle
    // above (normalise, apply to this player, persist to settings, republish
    // all three labels in the same turn) - only the target differs.
    //
    // The OFA row is refused where the runtime is missing, exactly like the
    // cycle's skip: `set_interp_mode(3)` would report a mode the pacer can
    // never run, and the user would see "OFA" over a blended picture.
    {
        let control = control.clone();
        let window = weak.clone();
        let store = settings_store.clone();
        player.on_interp_set(move |mode| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let wanted = mode.clamp(0, 3) as u8;
            if wanted == player::INTERP_MODE_OFA && !player::ofa_present() {
                crate::debug_log("interp menu skip mode=3 reason=dll_missing");
                return;
            }
            control.set_interp_mode(wanted);
            update_settings(&store, |prefs| prefs.interp_mode = wanted);
            window.set_interp_mode_text(control.interp_mode_text().into());
            window.set_interp_mode(i32::from(control.interp_mode()));
            window.set_interp_stats_text(control.interp_stats_text().into());
            crate::debug_log(&format!(
                "interp menu set mode={} text={} source=menu",
                control.interp_mode(),
                control.interp_mode_text()
            ));
        });
    }
    // 帧率 row of the 平滑 menu: 60 / 120 for the *output grid*.
    //
    // 120 is not a promise the UI can make on its own, so it is measured: the
    // switch requests a restart (a seek to the current position, the same path
    // a 画质 pick uses), waits for the new session to settle, then reads the
    // present intervals it produced for three seconds. A grid that cannot hold
    // an 8.3 ms slot (p95 over one 120 Hz frame) is refused: 120 is turned back
    // off, the row stays unavailable for this window, and both the log and the
    // toast carry the measured p95 as the reason.
    {
        let control = control.clone();
        let window = weak.clone();
        let store = settings_store.clone();
        let ok = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        player.on_interp_fps(move |on| {
            let Some(window) = window.upgrade() else {
                return;
            };
            if control.interp_mode() == 0 && on {
                crate::debug_log("interp fps ignored hz=120 reason=mode_off");
                return;
            }
            if busy.load(Ordering::SeqCst) {
                return;
            }
            if !on {
                control.set_interp_120(false);
                ok.store(true, Ordering::SeqCst);
                update_settings(&store, |prefs| prefs.interp_120 = false);
                window.set_interp_120(false);
                window.set_interp_120_ok(true);
                window.set_interp_120_note(SharedString::default());
                // The grid applies to the next session: restart this one.
                control.seek(control.playback_position_ms());
                crate::debug_log("interp fps set hz=60 source=menu");
                return;
            }
            if !ok.load(Ordering::SeqCst) {
                crate::debug_log("interp fps refused hz=120 reason=self_check_failed");
                return;
            }
            busy.store(true, Ordering::SeqCst);
            control.set_interp_120(true);
            update_settings(&store, |prefs| prefs.interp_120 = true);
            window.set_interp_120(true);
            crate::debug_log("interp fps set hz=120 source=menu check=armed");
            // The restart is what makes the pacer resolve a 120 Hz grid.
            control.seek(control.playback_position_ms());
            let control_probe = control.clone();
            let window_probe = window.as_weak();
            let ok_probe = ok.clone();
            let busy_probe = busy.clone();
            let store_probe = store.clone();
            std::thread::spawn(move || {
                // 1.2 s for the restart to reach its first picture, then a 3 s
                // window of present intervals.
                std::thread::sleep(Duration::from_millis(1_200));
                std::thread::sleep(Duration::from_millis(3_000));
                let (p50, p95, samples) = control_probe.present_percentiles();
                let out_hz = control_probe.interp_out_hz();
                let (shown_surface, blit_us) = control_probe.surface_receipt();
                // The threshold is one 120 Hz slot plus 2 %: an interval p95
                // above it means the grid misses frames, and the mode would be
                // advertised with a rate it cannot hold.
                let threshold = 8.5;
                let passed = out_hz >= 120 && samples >= 60 && p95 <= threshold;
                // `shown_s` is derived from the measured interval: that is the
                // rate pictures actually reached the screen at during the
                // check, which is the number the mode is judged on (and the one
                // the log keeps for every entry into 120).
                let shown_s = if p50 > 0.0 { 1000.0 / p50 } else { 0.0 };
                crate::debug_log(&format!(
                    "interp fps check hz=120 out_hz={out_hz} passed={} shown_s={shown_s:.1} p50_ms={p50:.2} p95_ms={p95:.2} n={samples} threshold_ms={threshold:.2} surface_shown={shown_surface} blit_us={blit_us}",
                    u8::from(passed)
                ));
                let _ = slint::invoke_from_event_loop(move || {
                    busy_probe.store(false, Ordering::SeqCst);
                    let Some(window) = window_probe.upgrade() else {
                        return;
                    };
                    if passed {
                        ok_probe.store(true, Ordering::SeqCst);
                        window.set_interp_120_ok(true);
                        window.set_interp_120_note(SharedString::default());
                        toast(
                            &window,
                            format!("插帧 120：{shown_surface} 帧/秒（实测）"),
                            TOAST_KIND_SUCCESS,
                        );
                        return;
                    }
                    // Refused: put the setting back and say exactly why.
                    ok_probe.store(false, Ordering::SeqCst);
                    control_probe.set_interp_120(false);
                    update_settings(&store_probe, |prefs| prefs.interp_120 = false);
                    window.set_interp_120(false);
                    window.set_interp_120_ok(false);
                    let note = format!("120 档上屏不达标：p95 {p95:.1} ms（需 ≤ 8.3）");
                    window.set_interp_120_note(note.clone().into());
                    toast(&window, format!("已保持 60 fps · {note}"), TOAST_KIND_INFO);
                });
            });
        });
    }
    // Everything that belongs to exactly this window. The session is registered
    // before the callbacks below, which resolve it by HWND.
    let session_rec = Rc::new(PlayerSession {
        id: next_player_session_id(),
        window: player.clone_strong(),
        control: control.clone(),
        panel_geom: panel_geom.clone(),
        hwnd: Cell::new(win::hwnd_of(player.window()).unwrap_or(0)),
        closed: Cell::new(false),
        loops: RefCell::new(None),
        mini_slot: Arc::new(Mutex::new(None)),
        video: video.clone(),
        cascade,
        queued: Cell::new(false),
        deferred: RefCell::new(None),
        live: false,
    });
    // The window's own callbacks capture this slot, so the pointer watch lives
    // exactly as long as THIS window even if the session registry changes.
    let loops_slot: Rc<RefCell<Option<Rc<PlayerLoops>>>> = Rc::new(RefCell::new(None));
    state.players.borrow_mut().push(session_rec.clone());
    crate::debug_log(&format!(
        "player session open win={} hwnd={} players={}",
        session_rec.id,
        session_rec.hwnd.get(),
        state.players.borrow().len()
    ));
    let track = Arc::new(player::DanmakuTrack::new());
    let sleep_mode = Arc::new(AtomicU64::new(0));
    let sleep_deadline = Arc::new(AtomicU64::new(0));
    let clock = Instant::now();
    // The danmaku model is created once here and mutated in place by the tick, so
    // a frame diffs rows instead of re-creating every danmaku node each tick.
    //
    // f196: on the composite path the comments belong to the DComp danmaku layer
    // (`src/dcomp/danmaku.rs`), and the motion lane must publish into THAT
    // model - the player component's own `danmaku_items` stays empty, which is
    // what keeps a moving comment from dirtying the UI layer at all.
    let external_danmaku = win::hwnd_of(player.window())
        .and_then(crate::dcomp::danmaku_model);
    let danmaku_model = match &external_danmaku {
        Some(model) => model.clone(),
        None => Rc::new(VecModel::<DanmakuItem>::from(Vec::<DanmakuItem>::new())),
    };
    if external_danmaku.is_some() {
        player.set_external_danmaku(true);
    } else {
        player.set_danmaku_items(ModelRc::from(danmaku_model.clone()));
    }
    // 弹幕 motion lane: the fast tick (see the "弹幕 motion lane" section) owns the
    // x of the comments already on screen; the 33 ms tick below owns everything
    // else and publishes the visible set plus the media-clock anchor. The rate
    // is the display's refresh capped at 120 Hz, overridable through
    // `BILI_DANMAKU_HZ` (30/60/120) for the A/B.
    let display_hz = display_refresh_hz();
    let (danmaku_hz, danmaku_hz_source) = danmaku_frame_hz(display_hz);
    let danmaku_layer = Rc::new(DanmakuLayer::new(
        player.as_weak(),
        danmaku_model.clone(),
        danmaku_hz,
    ));
    player.set_danmaku_frame_interval(danmaku_wake_ms(danmaku_hz));
    // f196 QA: `BILI_F196_CAPTURE` snapshots this window's Slint tree (see
    // src/qa/f196.rs); inert without the variable.
    crate::qa::f196_spawn_capture(&player.as_weak());
    // f200 QA: `BILI_F200_LAYERS=<seconds>` prints the three layers' per-second
    // rates (see src/qa/f200.rs); inert without the variable.
    crate::qa::f200_spawn_layers();
    // f201 QA: `BILI_F201_TEST=<seconds>[,pause|idle]` opens the rewritten 性能
    // panel and reads its rows back once a second (see src/qa/f201.rs); inert
    // without the variable.
    crate::qa::f201_spawn_panel(player.as_weak());
    player.set_danmaku_frame_running(false);
    // The panel's refresh rate, captured for the 插帧-120 cap in the slow tick.
    let danmaku_display_hz = display_hz;
    crate::debug_log(&format!(
        "danmaku frame open hz={danmaku_hz} display_hz={display_hz} source={danmaku_hz_source} \
         interval_ms={:.2} wake_ms={}",
        danmaku_frame_interval_ms(danmaku_hz),
        danmaku_wake_ms(danmaku_hz)
    ));
    // 缩放进度条 heat: one 20-cell model per player window, replaced in place by
    // `publish_scrub_heat` on a zoom gesture. A fresh window starts empty (level
    // 0), which is also the reset a "different video" needs.
    let scrub_heat = Rc::new(VecModel::<f32>::from(Vec::<f32>::new()));
    player.set_scrub_heat(ModelRc::from(scrub_heat.clone()));
    // The row-id list lives inside `DanmakuLayer` (it is per player: several
    // player windows can be open at once, and a list shared through
    // `PlayerState` let each window's tick delete and re-append the other
    // window's rows — the flicker the user saw on the second video).
    // Set by any callback that changes what the tick renders while paused, so a
    // paused-but-edited frame still repaints (see the skip guard below).
    let refresh = Rc::new(Cell::new(true));
    let last_position = Rc::new(Cell::new(u64::MAX));
    let last_danmaku_on = Rc::new(Cell::new(true));
    // f207: the wake the danmaku timer currently holds, so the strips path can
    // raise it once without re-arming the timer on every tick.
    let last_danmaku_wake = Rc::new(Cell::new(danmaku_wake_ms(danmaku_hz)));
    // f196: one-shot log of the player's HWND (see the danmaku-visibility block
    // in the tick).
    let hwnd_logged = Rc::new(Cell::new(false));
    // Last duration written to the window: constant for a track, so the total
    // label is formatted once per track instead of on every 33ms tick.
    let last_duration = Rc::new(Cell::new(u64::MAX));
    // Last opacity baked into the model rows; a change repaints even while
    // paused (see the skip guard). -1.0 is never a valid opacity.
    let last_opacity = Rc::new(Cell::new(-1.0f32));
    // Gate for the once-a-second `danmaku diff` debug-log line.
    let last_diff_log = Rc::new(Cell::new(0u64));
    // 网络分析: the two models Rust patches in place (线程 rows and the
    // 120-slice 下载分布 strip) plus the 5 s rate window behind the live bar.
    let net_panel = Rc::new(NetPanel::new());
    net_panel.attach(&player);
    let net_last_publish = Rc::new(Cell::new(-NET_PUBLISH_MS));
    // 性能面板: the per-second instrument (see the `PerfPanel` section above).
    // It owns the present/push deltas, the 弹幕 fast-lane tick count and the
    // rolling cost windows, and writes the `perf_*` properties once a second -
    // never per frame, so a busy panel cannot cost frames.
    let perf_panel = Rc::new(PerfPanel::new());
    let perf_last_publish = Rc::new(Cell::new(-PERF_PUBLISH_MS));
    // Gate for the once-a-second `prefetch stats` line (per-host split).
    let net_last_split = Rc::new(Cell::new(-NET_SPLIT_LOG_MS));
    // `BILI_BUFFER_TRACE=1`: log the first 500 ms of `buffering` / position at
    // 50 ms, so the pre-show buffering state can be proven frame by frame.
    let buffer_trace = Rc::new(
        std::env::var("BILI_BUFFER_TRACE")
            .map(|value| value.trim() == "1")
            .unwrap_or(false),
    );
    let buffer_trace_at = Rc::new(Cell::new(-50i64));
    // Last `(buffering, picture)` pair published to the UI, so the QA flag can
    // log the invariant on every transition instead of only the first 500 ms.
    let buffer_state = Rc::new(Cell::new((true, false)));
    // Last `buffer_detail` string pushed, so the property is only written when
    // the measured line changed (and the change is logged as a sample).
    let buffer_detail_last = Rc::new(RefCell::new(String::new()));
    // 4 Hz gate for recomputing that line (the proxy snapshot takes a lock).
    let buffer_detail_at = Rc::new(Cell::new(-NET_PUBLISH_MS));

    player.set_danmaku_running(true);

    // The window grows/shrinks to the strips the UI reserved. The ints are the
    // only input - Rust never writes them.
    {
        let geom = panel_geom.clone();
        let window = weak.clone();
        player.on_panel_changed(move || {
            if let Some(window) = window.upgrade() {
                apply_panel_geometry(
                    &geom,
                    window.window(),
                    window.get_left_panel(),
                    window.get_right_panel(),
                    window.get_bottom_panel(),
                    false,
                );
            }
        });
    }

    // 缩放进度条: the UI derives the window (base/span) from `scrub_level` and
    // fires this after every real change. Answer with 20 densities for that
    // window from the curve the seek bar already has; level 0 clears the model.
    {
        let window = weak.clone();
        let scrub_heat = scrub_heat.clone();
        player.on_scrub_level_changed(move |level| {
            if let Some(window) = window.upgrade() {
                publish_scrub_heat(&window, &scrub_heat, level);
            }
        });
    }

    {
        let control = control.clone();
        let track = track.clone();
        let window = weak.clone();
        let state = state.clone();
        let danmaku_layer = danmaku_layer.clone();
        let sleep_mode = sleep_mode.clone();
        let sleep_deadline = sleep_deadline.clone();
        let refresh = refresh.clone();
        let last_opacity = last_opacity.clone();
        let last_duration = last_duration.clone();
        let last_diff_log = last_diff_log.clone();
        let panel_geom = panel_geom.clone();
        let net_panel = net_panel.clone();
        let net_last_publish = net_last_publish.clone();
        let perf_panel = perf_panel.clone();
        let perf_last_publish = perf_last_publish.clone();
        let net_last_split = net_last_split.clone();
        let buffer_trace = buffer_trace.clone();
        let buffer_trace_at = buffer_trace_at.clone();
        let buffer_state = buffer_state.clone();
        let bvid_log = bvid.clone();
        // The merged 画质 menu is published from the **decoder's own readback**
        // (`PlaybackControl::current_stream`, the `stream qn=` line), and
        // republished whenever that stream changes. Marking the row a click
        // asked for instead would let the menu claim 4K while the decoder runs
        // 1080P (an exact pick whose representation vanished, a fallback, a
        // restart that landed elsewhere) — the one thing this menu must never
        // do. Per window, so a second player never suppresses the first one's
        // publish.
        let quality_menu_stream = Rc::new(Cell::new(None));
        let quality_menu_stream_tick = quality_menu_stream.clone();
        // Keeps this window's own pointer watch/layout debounce alive for as
        // long as this window's tick exists (dropping the timer would stop it).
        let loops_keepalive = loops_slot.clone();
        let buffer_detail_last = buffer_detail_last.clone();
        let buffer_detail_at = buffer_detail_at.clone();
        // 继续观看: this window's only progress writer. Re-armed by every source
        // switch; it fires once the new episode's first frame is on screen.
        let progress_hook = pgc.as_ref().map(|pgc| {
            Rc::new(PgcProgressHook {
                season_id: pgc.season.season_id,
                episodes: pgc.season.episodes.clone(),
                seq: Cell::new(u64::MAX),
                armed: Cell::new(true),
                written: Cell::new(0),
            })
        });
        player.on_danmaku_tick(move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            let _ = &loops_keepalive;
            // 提示条: expire/republish the toast queue. Self-gated to ~250ms, and
            // called before the paused-frame skip guard so a paused player's
            // toasts still expire.
            toast_tick(&window);
            // 画质 menu: the player installs its stream catalogue and the
            // stream-only model in one event-loop step, so a non-empty
            // `quality_rows` means the catalogue is ready. The marked row is the
            // stream the decoder really opened; the menu is republished on every
            // change of it (a switch, a fallback, a new episode), so the
            // highlight and the `stream qn=` readback can never disagree.
            if window.get_quality_rows().row_count() > 0 {
                let actual = control.current_stream();
                if actual.is_some() && actual != quality_menu_stream_tick.get() {
                    quality_menu_stream_tick.set(actual);
                    if let Some((qn, codecid, bandwidth)) = actual {
                        let specs = control.quality_catalog();
                        // The tiers belong to the video the decoder is on, not
                        // to the one this window was opened with: a 选集 switch
                        // re-arms the source in the same window.
                        let source = control.source();
                        let tiers =
                            bilibili::quality_tiers_for(&source.bvid, source.cid).unwrap_or_default();
                        publish_quality_menu(
                            &window,
                            &specs,
                            &tiers,
                            Some((qn, codecid, bandwidth)),
                        );
                        // The 画质 row's own value follows the same readback.
                        if window.get_quality_qn() != qn as i32 {
                            window.set_quality_qn(qn as i32);
                        }
                        let label = bilibili::quality_label(qn);
                        if window.get_quality_label().as_str() != label {
                            window.set_quality_label(label.into());
                        }
                    }
                }
            }
            // 帧插值: mode badge, measured cost and (when an NVIDIA GPU is
            // present) its utilisation. Written only on change: the tick runs at
            // ~30 Hz and a Slint property write repaints the row.
            let interp_mode_text = control.interp_mode_text();
            if window.get_interp_mode_text().as_str() != interp_mode_text {
                window.set_interp_mode_text(interp_mode_text.into());
            }
            let interp_mode = i32::from(control.interp_mode());
            if window.get_interp_mode() != interp_mode {
                window.set_interp_mode(interp_mode);
            }
            let interp_stats_text = control.interp_stats_text();
            if window.get_interp_stats_text().as_str() != interp_stats_text {
                window.set_interp_stats_text(interp_stats_text.into());
            }
            let interp_gpu_text = gpu_utilization_text();
            if window.get_interp_gpu_text().as_str() != interp_gpu_text {
                window.set_interp_gpu_text(interp_gpu_text.into());
            }
            // f202: 画质增强. The tier is a process-wide atomic (the video
            // thread reads it once per frame), so this tick is what makes a
            // fallback visible: when a tier fails it clears the atomic, and the
            // row snaps back to 关闭 with the failure as its hint instead of
            // showing a tier that is no longer running.
            let enhance_mode = crate::dcomp::enhance::desired() as i32;
            if window.get_enhance_mode() != enhance_mode {
                window.set_enhance_mode(enhance_mode);
                // A paused picture produces no frames, so the tier change has to
                // be presented explicitly or the menu would look dead until
                // playback resumes.
                if let Some(hwnd) = crate::win::hwnd_of(window.window()) {
                    crate::dcomp::repaint(hwnd);
                }
            }
            if let Some(reason) = crate::dcomp::enhance::last_failure() {
                if window.get_enhance_hint().as_str() != reason {
                    window.set_enhance_hint(reason.into());
                }
            }
            // 播放列表: this window is the queue's active player, so report the
            // play/pause state and watch for the end of the track (self-gated
            // to 250ms, like the toast tick).
            if let Some(host) = playlist() {
                host.tick(&window, &control);
            }
            // 外扩面板: the ints are the single source of truth. The UI's own
            // toggle functions fire `panel_changed`, and this reconciliation
            // also catches a property written directly (BILI_PANEL_TEST), so a
            // panel can never be laid out without its window growing.
            apply_panel_geometry(
                &panel_geom,
                window.window(),
                window.get_left_panel(),
                window.get_right_panel(),
                window.get_bottom_panel(),
                false,
            );
            // f194: the DComp video layer is placed from the same properties the
            // UI lays the band out with. Publishing it here (not only with a
            // frame) keeps the picture in the right slot while paused, when a
            // panel that changes the band opens without a frame arriving.
            if let Some(hwnd) = win::hwnd_of(window.window()) {
                let (fw, fh) = (control.frame_width(), control.frame_height());
                // The transparent hole the DComp picture shows through must
                // have the decoded frame's aspect, not a fixed 16:9 (4:3 and
                // 21:9 sources otherwise leak the desktop through the bars).
                let aspect = if fw > 0 && fh > 0 {
                    fw as f32 / fh as f32
                } else {
                    16.0 / 9.0
                };
                if (window.get_video_aspect() - aspect).abs() > 0.001 {
                    window.set_video_aspect(aspect);
                }
                crate::dcomp::sync_layout(
                    hwnd,
                    crate::dcomp::VideoLayout {
                        band: (
                            window.get_surface_x(),
                            window.get_surface_y(),
                            window.get_surface_w(),
                            window.get_surface_h(),
                        ),
                        scale: window.window().scale_factor(),
                        fit: window.get_fit_mode().clamp(0, 2) as u8,
                        frame: if fw > 0 && fh > 0 {
                            (fw, fh)
                        } else {
                            (960, 540)
                        },
                    },
                );
            }
            // 网络分析: the profiler is sampled every tick but only formatted
            // at 4 Hz; one sparkline sample per second. Both snapshots are taken
            // inside the 4 Hz block below: `prefetch_stats` locks the proxy and
            // merges/clones its range bookkeeping, which is far too much work
            // for a 33ms tick.
            // Pre-roll gate (src/player.rs): true while the pacer holds the
            // first frame. On top of that the layer must stay up while no
            // picture exists at all (between `open` and the gate arming), so
            // the first painted frame is never the bare tint. 仅音频
            // has no picture by design and is therefore exempt.
            let net_now = clock.elapsed().as_millis() as i64;
            // A queued window has no decoder yet: it shows the explicit queue
            // state, never the buffering plate over a black frame.
            let queued = window.get_playback_queued();
            // 插帧 120: the picture is on its own surface, so `frame` stays
            // 0x0 — the plate must not read that as "no picture at all" and
            // stay up over a video that is playing.
            let has_picture = control.audio_only()
                || window.get_external_video()
                || window.get_frame().size().width > 0;
            let gate = control.buffering();
            let buffering = !queued && (gate || !has_picture || window.get_qa_force_buffering());
            window.set_buffering(buffering);
            window.set_seek_hold(control.seek_freeze());
            window.set_buffer_progress(if gate {
                control.buffer_progress()
            } else if buffering {
                0.0
            } else {
                1.0
            });
            // 继续观看: write 「看到第几集」 the moment this episode has a picture
            // (first frame presented, progress 0 is fine). One write per episode:
            // the store is a small table, not a history, and the 番剧 cards
            // behind the player are repainted from it right away.
            if let Some(hook) = progress_hook.as_ref() {
                let seq = control.source_seq();
                if seq != hook.seq.get() {
                    hook.seq.set(seq);
                    hook.armed.set(true);
                }
                if hook.armed.get() && !buffering && has_picture && !queued {
                    let source = control.source();
                    if let Some(position) = hook
                        .episodes
                        .iter()
                        .position(|episode| episode.cid > 0 && episode.cid == source.cid)
                    {
                        hook.armed.set(false);
                        if hook.written.get() != source.cid {
                            hook.written.set(source.cid);
                            let episode = &hook.episodes[position];
                            crate::debug_log(&format!(
                                "pgc progress arm season={} ep={} cid={} frame={}x{} pos_ms={}",
                                hook.season_id,
                                episode.ep_id,
                                source.cid,
                                window.get_frame().size().width,
                                window.get_frame().size().height,
                                control.playback_position_ms()
                            ));
                            if pgc_write_progress(hook.season_id, episode, position as u32 + 1)
                                .is_some()
                            {
                                pgc_refresh_badges();
                            }
                        }
                    }
                }
            }
            // The quantified line under the loading animation: measured proxy
            // throughput/read-ahead (see `PlaybackControl::buffer_detail`), only
            // while the overlay is up. Recomputed at 4 Hz (the proxy snapshot
            // takes a lock) and written on change, so a loading seek logs a
            // handful of honest samples instead of 60 identical ones.
            let mut detail = buffer_detail_last.borrow().clone();
            if buffering {
                if net_now - buffer_detail_at.get() >= 250 {
                    buffer_detail_at.set(net_now);
                    detail = control.buffer_detail();
                }
            } else {
                detail.clear();
            }
            if *buffer_detail_last.borrow() != detail {
                *buffer_detail_last.borrow_mut() = detail.clone();
                window.set_buffer_detail(detail.clone().into());
                crate::debug_log(&format!(
                    "buffer detail t_ms={net_now} buffering={buffering} seek={} text={}",
                    control.seek_buffering(),
                    if detail.is_empty() { "-" } else { detail.as_str() }
                ));
            }
            if *buffer_trace {
                trace_buffer_state(&window, &control, &buffer_trace_at, net_now);
                if buffer_state.get() != (buffering, has_picture) {
                    buffer_state.set((buffering, has_picture));
                    crate::debug_log(&format!(
                        "buffer state t_ms={net_now} buffering={buffering} picture={has_picture} position_ms={}",
                        control.playback_position_ms()
                    ));
                }
            }
            if net_now - net_last_publish.get() >= NET_PUBLISH_MS {
                net_last_publish.set(net_now);
                // Metered counters come from the loopback proxy (`None` while it
                // is not feeding this decode session, so the panel shows the
                // estimate); sampled here, at the publish rate.
                let stats = control.play_stats();
                let prefetch_stats = control.prefetch_stats();
                let counts =
                    net_panel.publish(&window, stats.as_ref(), prefetch_stats.as_ref(), net_now);
                // `BILI_NET_TRACE=1`: prove the strip is driven by the live
                // proxy - the published slice counts, once per publish tick,
                // for the first 3 s.
                if net_panel.trace_ms > 0 && net_now <= net_panel.trace_ms {
                    let (workers, active) = prefetch_stats
                        .as_ref()
                        .map(|metered| (metered.workers.len(), metered.active_workers))
                        .unwrap_or((0, 0));
                    crate::debug_log(&format!(
                        "net map t_ms={net_now} slices={} missing={} downloaded={} active={} \
                         workers={active}/{workers} rate={}",
                        NET_MAP_SLICES, counts[0], counts[1], counts[3],
                        window.get_net_rate()
                    ));
                }
                // The per-host split has no property of its own; it goes to the
                // diagnostic log once a second.
                if let Some(metered) = prefetch_stats.as_ref() {
                    if net_now - net_last_split.get() >= NET_SPLIT_LOG_MS {
                        net_last_split.set(net_now);
                        log_prefetch_split(&bvid_log, metered);
                    }
                }
            }
            let position = control.playback_position_ms();
            let paused = control.paused();
            let danmaku_on = window.get_danmaku_on();
            // Read (and clamp) the layer opacity here: the danmaku layer no
            // longer binds to it, the tick bakes it into each item's colour.
            // Comparing it below keeps a slider move repainting while paused.
            let opacity = window.get_danmaku_opacity().clamp(0.0, 1.0);
            let mode = sleep_mode.load(Ordering::SeqCst);
            // The fast lane may only run while comments can actually move: a
            // paused, buffering or 弹幕-off window must not wake the UI thread
            // 120 times a second for nothing. Reading the media clock back here
            // also stops the between-frames extrapolation, so a stalled clock
            // never creeps ahead of the picture.
            let frames_on = !paused && danmaku_on && !buffering;
            if window.get_danmaku_frame_running() != frames_on {
                window.set_danmaku_frame_running(frames_on);
            }
            // 性能面板: one publish per second, from counters only. Placed after
            // `frames_on` (the 弹幕 rows need to know whether the lane could
            // run at all) and before the paused-frame skip guard below, so a
            // pause still publishes the second it happened.
            if net_now - perf_last_publish.get() >= PERF_PUBLISH_MS {
                perf_last_publish.set(net_now);
                // The proxy snapshot takes a lock and merges range
                // bookkeeping; at 1 Hz it is the same work the 网络分析 publish
                // already does 4x a second, so one more sample per second is
                // cheap and keeps the two panels on one source.
                let perf_stats = control.play_stats();
                let perf_prefetch = control.prefetch_stats();
                perf_panel.publish(
                    &window,
                    perf_stats.as_ref(),
                    perf_prefetch.as_ref(),
                    &control,
                    PerfCtx {
                        danmaku_on,
                        paused,
                        frames_on,
                        dm_rows: danmaku_layer.model.row_count(),
                    },
                    net_now,
                );
            }
            // f196: the composite path draws the comments in their own layer,
            // so the switch has to reach that layer too (the in-band copy hides
            // itself through `danmaku_on`).
            // f196: the composite path draws the comments in their own layer,
            // so the switch has to reach that layer too (the in-band copy hides
            // itself through `danmaku_on`). The resolved HWND is logged once so
            // a QA capture can address the player window on both platforms
            // (winit does not report one at session creation).
            if let Some(hwnd) = win::hwnd_of(window.window()) {
                if !hwnd_logged.get() {
                    hwnd_logged.set(true);
                    crate::debug_log(&format!(
                        "player hwnd={hwnd} class={}",
                        if crate::dcomp::is_player_window(hwnd) {
                            "dcomp"
                        } else {
                            "winit"
                        }
                    ));
                }
                crate::dcomp::set_danmaku_visible(hwnd, danmaku_on);
            }
            // 弹幕 绘制: the layer's own per-frame render cost, sampled at 20 Hz
            // while the lane runs. Reading it takes the layer registry lock, so
            // it is deliberately not a per-frame read; the panel's p50/p95 is
            // taken over the last 256 samples of that stream. The winit path has
            // no layer stats at all (`None`), and its 绘制 row comes from the
            // fast lane's own measured tick instead.
            if frames_on && net_now - perf_panel.dm_sample_at.get() >= 50 {
                perf_panel.dm_sample_at.set(net_now);
                if let Some(hwnd) = win::hwnd_of(window.window()) {
                    if let Some(dm) = crate::dcomp::danmaku_stats(hwnd) {
                        perf_panel.note_dm_render(dm.render_ms);
                    }
                }
            }
            // 插帧 120 (or the 4x grid): the picture already changes 120 times a
            // second, and every danmaku row write is a full-window repaint on
            // this window. Half the ticks carry the same picture, so the lane is
            // capped at 60 Hz whenever the pacer's output grid is at or above
            // that. The motion stays smooth (a scroll comment moves ~1.3 px per
            // 60 Hz tick) and the repaint count stops doubling for comments.
            //
            // f207: with the comment layer blitting cached masks
            // (`dcomp::strips`) a tick is no longer a window repaint, so this
            // cap was re-measured as a candidate to lift: at 120 Hz the lane did
            // reach ~110 presents/s, and the NVIDIA driver faulted inside
            // `nvwgf2umx.dll` (0xc0000005, faulting offset identical across
            // runs) within 10-20 s of the switch - twice, with and without the
            // frame-latency wait in front of the present. The cap stays at 60
            // because 120 presents/s of a third composition layer is not
            // something this driver survives, not because of the repaint cost.
            {
                let display_hz = danmaku_target_hz(control.interp_out_hz(), danmaku_display_hz);
                let strips_on = win::hwnd_of(window.window())
                    .and_then(crate::dcomp::danmaku_stats)
                    .is_some_and(|stats| stats.strips_on);
                if display_hz != danmaku_layer.hz() {
                    danmaku_layer.apply_hz(display_hz);
                    crate::debug_log(&format!(
                        "danmaku frame cap hz={display_hz} out_hz={} display_hz={danmaku_display_hz} \
                         strips={}",
                        control.interp_out_hz(),
                        u8::from(strips_on)
                    ));
                }
                // f207: the timer wakes every `1000 / hz` ms (whole ms, 16 ms for
                // 60 Hz) while the gate needs `1000 / hz` = 16.67 ms, so the
                // first wake is dropped and the lane lands at 42-56 ticks/s.
                // Giving the wake one extra millisecond lets every wake through
                // (~59/s) - measured on the strips path, where a tick costs
                // ~0.1 ms. The fallback keeps the old wake: there a tick is a
                // full software render.
                if strips_on {
                    let wake = danmaku_frame_interval_ms(display_hz).floor() as i64 + 1;
                    if last_danmaku_wake.get() != wake {
                        last_danmaku_wake.set(wake);
                        window.set_danmaku_frame_interval(wake);
                        crate::debug_log(&format!(
                            "danmaku frame wake ms={wake} hz={display_hz} strips=1"
                        ));
                    }
                }
            }
            if !frames_on {
                danmaku_layer.freeze();
            }
            // Paused with no countdown and nothing changed since the last frame:
            // every value below is identical, so skip the text writes and the
            // danmaku walk instead of repeating them 30x a second.
            if paused
                && mode == 0
                && position == last_position.get()
                && danmaku_on == last_danmaku_on.get()
                && opacity == last_opacity.get()
                && !refresh.get()
            {
                return;
            }
            last_position.set(position);
            last_danmaku_on.set(danmaku_on);
            last_opacity.set(opacity);
            refresh.set(false);
            let duration = control.duration_ms();
            if duration != last_duration.get() {
                last_duration.set(duration);
                window.set_duration_ms(duration as f32);
                window.set_time_total(format_mmss(duration).into());
            }
            // While the player is buffering the clock is not moving forward,
            // so the readouts hold instead of creeping to a position that has
            // no picture behind it yet.
            //
            // f151 exception: during a seek freeze the position *is* the held
            // target (`playback_position_ms` returns it), and the readout must
            // stay there while the overlay is up. Skipping it left the Slint
            // `position_ms` on the pre-release value, so the fill/readout
            // snapped back to the old playhead as soon as `scrub_settling`
            // expired - the "数字跳回去" half of the owner's shot.
            if !buffering || control.seek_freeze() {
                // The readout goes through the presentation rules: a seek must
                // jump at once (see `player_view::Presentation::seek`).
                crate::player_view::Presentation::new(&crate::player_view::SlintPlayerView(
                    window.as_weak(),
                ))
                .seek(position as u64);
                window.set_time_current(format_mmss(position).into());
                window.set_time_remaining(
                    format!("-{}", format_mmss(duration.saturating_sub(position))).into(),
                );
            }
            if let Some(mini) = mini_handle(&state) {
                mini.set_playing(!paused);
            }
            if mode != 0 {
                let now_ms = clock.elapsed().as_millis() as u64;
                let fire = if mode == 5 {
                    duration > 0 && position.saturating_add(800) >= duration
                } else {
                    now_ms >= sleep_deadline.load(Ordering::SeqCst)
                };
                if fire {
                    sleep_mode.store(0, Ordering::SeqCst);
                    control.set_paused(true);
                    window.set_playing(false);
                    window.set_sleep_text("睡眠定时已到，已暂停".into());
                } else if mode == 5 {
                    window.set_sleep_text("睡眠：播完当前视频".into());
                } else {
                    let remain = sleep_deadline.load(Ordering::SeqCst).saturating_sub(now_ms);
                    window.set_sleep_text(format!("睡眠：{}", fmt_countdown(remain)).into());
                }
            }
            if !danmaku_on {
                return;
            }
            let font_px = player::danmaku_font_px(control.danmaku_font());
            let (block_scroll, block_top, block_bottom) = control.type_blocks();
            let filter = player::DanmakuFilter {
                speed_percent: control.danmaku_speed(),
                area_percent: control.danmaku_area(),
                block_scroll,
                block_top,
                block_bottom,
                blocked_senders: control.blocked_senders(),
                block_rules: control.block_rules(),
            };
            let items = track.compute(position, font_px, &filter);
            // 弹幕 motion lane: the layer reconciles the model (add / update /
            // remove, opacity baked in) and re-anchors the fast tick's clock at
            // this position. The fast tick owns every x from here on, so a row
            // that survives keeps the position the motion clock gave it.
            let diff = danmaku_layer.publish(
                position as f32,
                items,
                opacity,
                filter.speed_percent,
                frames_on,
                Instant::now(),
            );
            let now_ms = clock.elapsed().as_millis() as u64;
            if now_ms.saturating_sub(last_diff_log.get()) >= 1000 {
                last_diff_log.set(now_ms);
                crate::debug_log(&format!(
                    "danmaku diff bvid={} updated={} inserted={} removed={} visible={}",
                    bvid_log, diff.updated, diff.inserted, diff.removed, diff.visible
                ));
            }
        });
    }

    {
        // The fast half: 8 ms (120 Hz), and it touches nothing but the x of the
        // rows already on screen. See the "弹幕 motion lane" section.
        //
        // 弹幕帧率: one tick here is one repaint request, so counting the ticks
        // is the honest frame rate of the comment lane (the 性能 panel reads the
        // delta once a second). The lane's 绘制 cost is not measured here: on
        // the winit path the comments are drawn inside the UI tree, so the only
        // real per-frame number is the layer's own (DComp) stats, and the panel
        // reports "—" where those do not exist.
        let layer = danmaku_layer.clone();
        let perf_tick = perf_panel.clone();
        player.on_danmaku_frame_tick(move || {
            player::probe_danmaku_tick(|| layer.frame_tick());
            perf_tick.note_dm_tick();
        });
    }

    {
        // 性能面板 4 ms probe: the 性能 body's timer calls this while it is on
        // screen (see `perf_ui_tick` in ui/app.slint), and each call records the
        // gap to the previous one. One Instant and one bucket per wakeup; the
        // panel reads the result in its 1 Hz publish.
        let perf_probe = perf_panel.clone();
        player.on_perf_ui_tick(move || perf_probe.note_ui_tick());
    }

    {
        // f198: the 60 Hz presentation lane (the `progress_tick` timer in
        // ui/app.slint, armed only while the chrome is on screen). It owns
        // nothing: the playhead is a function of the same clock the pacer uses,
        // so one read per tick is the whole input. The 33 ms tick keeps owning
        // the comment set, the panels and the toasts; this lane only moves what
        // the pill displays, which is what lets the fill and the two time
        // labels follow a 120 Hz picture instead of stepping at 30 Hz.
        let control = control.clone();
        let window = weak.clone();
        let last_at = Rc::new(Cell::new(u64::MAX));
        player.on_progress_tick(move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            // A frozen picture (pause, a seek in flight, buffering) keeps the
            // readout where it is. `seek_freeze` is checked because the 33 ms
            // tick writes the held target itself, and a lane that overtook it
            // would show the old position again.
            if control.paused() || control.buffering() || control.seek_freeze() {
                last_at.set(u64::MAX);
                return;
            }
            let position = control.playback_position_ms();
            if position == last_at.get() {
                return;
            }
            last_at.set(position);
            let duration = control.duration_ms();
            // The readout goes through the presentation rules like the content
            // tick does (see `player_view::Presentation::seek`): a seek jumps,
            // playback moves.
            crate::player_view::Presentation::new(&crate::player_view::SlintPlayerView(
                window.as_weak(),
            ))
            .seek(position as u64);
            window.set_time_current(format_mmss(position).into());
            window.set_time_remaining(
                format!("-{}", format_mmss(duration.saturating_sub(position))).into(),
            );
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
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_take_screenshot(move || {
            // The decoded frame is display-sized now, so its dimensions come
            // from the session that produced it — never a fixed 960x540.
            let Some((bytes, width, height)) = control.snapshot_frame() else {
                if let Some(window) = window.upgrade() {
                    toast(&window, "暂无画面可截图", TOAST_KIND_ERROR);
                }
                return;
            };
            let window = window.clone();
            std::thread::spawn(move || {
                let path = screenshot_path();
                let saved = image::RgbaImage::from_raw(width, height, bytes)
                    .map(|img| img.save(&path));
                let (message, kind) = match saved {
                    Some(Ok(())) => (
                        format!("已保存截图：{}", path.display()),
                        TOAST_KIND_SUCCESS,
                    ),
                    Some(Err(err)) => (format!("截图保存失败：{err}"), TOAST_KIND_ERROR),
                    None => ("截图数据异常".to_string(), TOAST_KIND_ERROR),
                };
                crate::debug_log(&format!("screenshot {message}"));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        toast(&window, message, kind);
                    }
                });
            });
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_toggle_audio_only(move || {
            let on = control.toggle_audio_only();
            if let Some(window) = window.upgrade() {
                window.set_audio_only(on);
                toast(
                    &window,
                    if on {
                        "仅音频：已停止画面，继续播放声音"
                    } else {
                        "已恢复画面"
                    },
                    TOAST_KIND_INFO,
                );
            }
        });
    }
    // QA (`BILI_AUDIO_ONLY_TEST=<delay_ms>`): flips 仅音频 through this same
    // callback, measures the sound and captures the two states (f184).
    crate::qa::maybe_start_audio_only_test(player.as_weak(), control.clone());
    // QA (`BILI_SR_QA=1`): drives 画质增强 through the same process-wide tier the
    // menu writes and logs each tier's GPU cost, presents/s and drops.
    crate::qa::maybe_start_sr_test(player.as_weak(), control.clone());
    // f204: open the 画质增强 menu (and optionally one tier) for a capture of
    // the 画面直出 row; no-op without `BILI_F204_MENU`.
    crate::qa::maybe_open_enhance_menu(player.as_weak());
    // f202: 画质增强. The pick goes to a process-wide atomic the video thread
    // reads on its next frame, is mirrored into the row (`enhance_mode`) and is
    // persisted, so a restart keeps the tier. The availability/hint publish at
    // open because nothing about it can change inside one session.
    //
    // f204: the 画面直出 row lives in the same section. It writes
    // `player_dcomp` and nothing else: `platform::enabled()` already resolved
    // this process's window class, so the row only shows "重启后生效" until the
    // next start. Never restart the app from here — that is the user's call.
    {
        let window = weak.clone();
        let store = settings_store.clone();
        let stored_dcomp = settings_store
            .lock()
            .map(|prefs| prefs.player_dcomp)
            .unwrap_or(false);
        if let Some(window) = window.upgrade() {
            publish_enhance(&window, stored_dcomp);
        }
        player.on_set_enhance(move |mode| {
            let mode = mode.clamp(0, 4) as u32;
            crate::dcomp::enhance::set_desired(mode);
            update_settings(&store, |prefs| prefs.player_enhance = mode);
            // The row mirrors the pick on its own; the tick sees the change and
            // repaints a paused picture (see the 画质增强 block there).
            if let Some(window) = window.upgrade() {
                if let Some(hwnd) = crate::win::hwnd_of(window.window()) {
                    crate::dcomp::repaint(hwnd);
                }
            }
        });
        let store = settings_store.clone();
        player.on_set_player_dcomp(move |on| {
            update_settings(&store, |prefs| prefs.player_dcomp = on);
            crate::debug_log(&format!(
                "f204 dcomp setting={} active={} (restart required)",
                if on { "on" } else { "off" },
                crate::dcomp::platform::enabled()
            ));
        });
    }
    {
        let sleep_mode = sleep_mode.clone();
        let sleep_deadline = sleep_deadline.clone();
        let window = weak.clone();
        player.on_set_sleep(move |minutes| {
            if minutes == 0 {
                sleep_mode.store(0, Ordering::SeqCst);
                if let Some(window) = window.upgrade() {
                    window.set_sleep_text(String::new().into());
                }
                return;
            }
            if minutes < 0 {
                sleep_mode.store(5, Ordering::SeqCst);
                if let Some(window) = window.upgrade() {
                    window.set_sleep_text("睡眠：播完当前视频".into());
                }
                return;
            }
            let ms = (minutes as u64).saturating_mul(60_000);
            sleep_deadline.store(clock.elapsed().as_millis() as u64 + ms, Ordering::SeqCst);
            sleep_mode.store(1, Ordering::SeqCst);
            if let Some(window) = window.upgrade() {
                window.set_sleep_text(format!("睡眠：{}", fmt_countdown(ms)).into());
            }
            crate::debug_log(&format!("sleep set minutes={minutes}"));
        });
    }
    // `toggle_info` used to flip the (now inert) `info_open` flag. The left
    // panel is `left_panel = 1` and is toggled by the UI itself, which calls
    // `panel_changed()`; Rust only reacts to that through `on_panel_changed`.
    {
        let control = control.clone();
        let refresh = refresh.clone();
        let window = weak.clone();
        player.on_seek(move |position| {
            let target = position.max(0.0) as u64;
            SEEK_REQUEST_COUNT.fetch_add(1, Ordering::SeqCst);
            crate::debug_log(&format!("seek requested ms={target}"));
            // The UI's `apply-seek` already published the target to the shown
            // position/fill/readout BEFORE this callback, so +0ms is the proof
            // that the release frame itself shows the target.
            let shown = window
                .upgrade()
                .map(|player| player.get_qa_shown_ms())
                .unwrap_or(-1.0);
            crate::debug_log(&format!(
                "seek display target={target} shown={shown:.0} +0ms real_ms={}",
                control.playback_position_ms()
            ));
            control.seek(target);
            // f151: the frozen-picture receipts. `pic=paused` means the frame
            // on screen is still the one the release froze (presented_frames
            // must not move between these samples); `pic=swapped` means the
            // new position's first frame is on screen. `pos_real` is the
            // content position of the pixel on screen, `pos_shown` the value
            // the fill/readout publish.
            if f151_probe_enabled() {
                let release_at = Instant::now();
                for delay in [50u64, 200, 400, 600] {
                    let window = window.clone();
                    let control = control.clone();
                    slint::Timer::single_shot(Duration::from_millis(delay), move || {
                        let Some(player) = window.upgrade() else {
                            return;
                        };
                        crate::debug_log(&format!(
                            "seek pic={} overlay={} pos_shown={:.0} pos_real={} frames={} target={target} at_ms={} slint_pos={:.0} settling={} slint_buffering={} control_buffering={}",
                            if control.seek_freeze() { "paused" } else { "swapped" },
                            u8::from(control.seek_buffering()),
                            player.get_qa_shown_ms(),
                            control.presented_position_ms(),
                            control.presented_frames(),
                            release_at.elapsed().as_millis(),
                            player.get_position_ms(),
                            u8::from(player.get_qa_seek_settling()),
                            u8::from(player.get_buffering()),
                            u8::from(control.buffering()),
                        ));
                    });
                }
            }
            // Sample the same display hold at +100/+600/+1200 ms. `shown` must
            // stay within 1 s of the target until the real playhead arrives.
            for delay in [100u64, 600, 1200] {
                let window = window.clone();
                let control = control.clone();
                slint::Timer::single_shot(Duration::from_millis(delay), move || {
                    let Some(player) = window.upgrade() else {
                        return;
                    };
                    crate::debug_log(&format!(
                        "seek display target={target} shown={:.0} +{delay}ms real_ms={}",
                        player.get_qa_shown_ms(),
                        control.playback_position_ms()
                    ));
                });
            }
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_volume_changed(move |volume| {
            control.set_volume(volume as i32);
            if let Some(window) = window.upgrade() {
                // Keep the UI value as the pointer produced it: the audio
                // control stores whole percent, so writing its rounded copy
                // back made a 240-move drag produce only ~70 visible steps.
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
        let control = control.clone();
        let window = weak.clone();
        player.on_set_speed(move |milli| {
            control.set_speed(milli as f64 / 1000.0);
            if let Some(window) = window.upgrade() {
                window.set_speed_milli((control.speed() * 1000.0).round() as f32);
                window.set_speed_text(format_speed(control.speed()).into());
            }
        });
    }
    {
        let geom = panel_geom.clone();
        let window = weak.clone();
        player.on_toggle_fullscreen(move || {
            if let Some(window) = window.upgrade() {
                let next = !window.window().is_fullscreen();
                if next {
                    // Capture the base size / picture position before the
                    // window covers the monitor; panels float over the picture
                    // while fullscreen and the window is never resized.
                    enter_panel_overlay(&geom, window.window());
                }
                window.window().set_fullscreen(next);
                window.set_fullscreen(next);
                window.set_panels_overlay(next);
                window.set_fullscreen_text(if next { "退出全屏" } else { "全屏" }.into());
                if !next {
                    // Winit restores the pre-fullscreen rect asynchronously;
                    // grow back by the strips that are open now afterwards.
                    schedule_overlay_exit(geom.clone(), window.as_weak(), 0);
                }
            }
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let bvid = bvid.clone();
        player.on_set_quality(move |qn| {
            control.set_quality(qn as i64);
            if let Some(window) = window.upgrade() {
                let changed = window.get_quality_qn() != qn;
                let label = bilibili::quality_label(qn as i64);
                window.set_quality_qn(qn);
                window.set_quality_label(label.clone().into());
                // `set_quality` dropped any exact pick, so mark the row of the
                // stream the automatic rule will open for this `qn`.
                let specs = control.quality_catalog();
                let tiers = bilibili::quality_tiers_for(&bvid, cid).unwrap_or_default();
                publish_quality_menu(
                    &window,
                    &specs,
                    &tiers,
                    auto_stream_for(&specs, qn as i64, codec_pref),
                );
                if changed {
                    toast(&window, format!("画质：{label}"), TOAST_KIND_INFO);
                }
            }
        });
    }
    {
        // 画质 menu: the row index names one exact raw DASH stream (`qn` alone
        // is ambiguous - 1080P60 ships as AVC / HEVC / AV1 at several
        // bitrates), or a locked 大会员 tier. Selecting the stream already
        // playing is a no-op in the control, so the decoder is not restarted
        // for nothing.
        let control = control.clone();
        let window = weak.clone();
        let bvid = bvid.clone();
        // One toast per open for the locked-tier explanation: clicking several
        // locked rows must not stack copies of the same message. The UI fires
        // `quality_menu_opened` on every fresh open, which re-arms the gate.
        let locked_notice = Rc::new(RefCell::new(LockedNotice::default()));
        {
            let locked_notice = locked_notice.clone();
            player.on_quality_menu_opened(move || {
                locked_notice.borrow_mut().opened();
                crate::debug_log("quality menu opened");
            });
        }
        player.on_quality_pick(move |index| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let specs = control.quality_catalog();
            let tiers = bilibili::quality_tiers_for(&bvid, cid).unwrap_or_default();
            let items = build_quality_menu(&specs, &tiers, None);
            let Some(item) = usize::try_from(index)
                .ok()
                .and_then(|index| items.get(index))
            else {
                crate::debug_log(&format!(
                    "quality pick index={index} ignored n={}",
                    items.len()
                ));
                return;
            };
            let Some(spec_index) = item.spec else {
                // Locked 大会员 tier: non-selectable. Log the tier and explain
                // once; the menu stays open (the UI does not close it).
                crate::debug_log(&format!(
                    "quality locked qn={} index={index}",
                    item.row.qn
                ));
                if locked_notice.borrow_mut().take() {
                    toast(&window, "该清晰度需要大会员", TOAST_KIND_INFO);
                }
                return;
            };
            let Some(spec) = specs.get(spec_index).cloned() else {
                return;
            };
            crate::debug_log(&format!(
                "quality pick index={index} qn={} codec={} bw={}",
                spec.qn, spec.codecid, spec.bandwidth
            ));
            let changed = !window
                .get_quality_menu_rows()
                .row_data(index as usize)
                .map(|row| row.current)
                .unwrap_or(false);
            if !changed {
                // The row the decoder is already on: no restart, no toast.
                crate::debug_log(&format!(
                    "quality pick index={index} already-current qn={}",
                    spec.qn
                ));
                return;
            }
            control.set_quality_exact(spec.qn, spec.codecid, spec.bandwidth);
            window.set_quality_qn(spec.qn as i32);
            window.set_quality_label(spec.label.clone().into());
            publish_quality_menu(
                &window,
                &specs,
                &tiers,
                Some((spec.qn, spec.codecid, spec.bandwidth)),
            );
            toast(&window, format!("画质：{}", spec.label), TOAST_KIND_INFO);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_danmaku_settings_changed(move |opacity, font| {
            let font = font.max(0) as u32;
            control.set_danmaku_font(font);
            let clamped = opacity.clamp(0.0, 1.0);
            let opacity_pct = (clamped * 100.0).round() as u32;
            if let Some(window) = window.upgrade() {
                window.set_danmaku_opacity(clamped);
                window.set_danmaku_font(font as i32);
                window.set_danmaku_opacity_text(opacity_pct.to_string().into());
            }
            update_settings(&settings_store, |prefs| {
                prefs.danmaku_opacity = opacity_pct;
                prefs.danmaku_font = font;
            });
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_danmaku_speed_changed(move |speed| {
            let percent = speed.max(0) as u32;
            control.set_danmaku_speed(percent);
            if let Some(window) = window.upgrade() {
                window.set_danmaku_speed(percent as i32);
            }
            update_settings(&settings_store, |prefs| prefs.danmaku_speed = percent.clamp(50, 200));
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_danmaku_area_changed(move |area| {
            let percent = area.max(0) as u32;
            control.set_danmaku_area(percent);
            if let Some(window) = window.upgrade() {
                window.set_danmaku_area(settings::clamp_area(percent) as i32);
            }
            update_settings(&settings_store, |prefs| prefs.danmaku_area = settings::clamp_area(percent));
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_danmaku_type_block_changed(move |scroll, top, bottom| {
            control.set_type_blocks(scroll, top, bottom);
            if let Some(window) = window.upgrade() {
                window.set_block_scroll(scroll);
                window.set_block_top(top);
                window.set_block_bottom(bottom);
            }
            update_settings(&settings_store, |prefs| {
                prefs.block_scroll = scroll;
                prefs.block_top = top;
                prefs.block_bottom = bottom;
            });
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_block_at(move |x, y| {
            let Some(window) = window.upgrade() else {
                return;
            };
            // f196: the hit test reads the model the comments are actually drawn
            // from - the layer's own model on the composite path, the player
            // component's on winit.
            let items = ModelRc::from(danmaku_model.clone());
            let mut hash = String::new();
            for index in 0..items.row_count() {
                let Some(item) = items.row_data(index) else {
                    continue;
                };
                if x >= item.x
                    && x <= item.x + item.w
                    && y >= item.y
                    && y <= item.y + item.size + 6.0
                {
                    hash = item.mid_hash.to_string();
                    break;
                }
            }
            crate::debug_log(&format!("block_at x={x:.0} y={y:.0} hash_len={}", hash.len()));
            if hash.is_empty() {
                return;
            }
            let snapshot = update_settings(&settings_store, |prefs| {
                if !prefs.blocked_senders.iter().any(|entry| entry == &hash) {
                    prefs.blocked_senders.push(hash);
                }
            });
            control.set_blocked_senders(snapshot.blocked_senders);
            refresh.set(true);
            toast(&window, "已屏蔽该发送者的弹幕", TOAST_KIND_SUCCESS);
        });
    }
    let liked_flag = Arc::new(AtomicBool::new(false));
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let window = weak.clone();
        let liked_flag = liked_flag.clone();
        crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
            let Some(runtime) = crate::bilibili::shared_runtime() else {
                return;
            };
            match runtime.block_on(session.like_state(&bvid)) {
                Ok(state) => {
                    let liked = state == 1;
                    crate::debug_log(&format!("like_state ok liked={liked}"));
                    liked_flag.store(liked, Ordering::SeqCst);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = window.upgrade() {
                            window.set_liked(liked);
                            window.set_like_text(if liked { "已赞" } else { "点赞" }.into());
                        }
                    });
                }
                Err(err) => crate::debug_log(&format!("like_state err={err}")),
            }
        }));
    }
    // f179: no UI entry calls this any more (the 互动 下载 chip is gone). The
    // handler stays wired so `download::spawn` and the shared download slot keep
    // their only caller: `src/download.rs` is kept, not deleted.
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let title = video.title.clone();
        let window = weak.clone();
        let weak_main = state.main.clone();
        let download_control = state.download_control.clone();
        player.on_download_current(move || {
            let control = download::DownloadControl::new();
            if let Ok(mut guard) = download_control.lock() {
                if let Some(old) = guard.take() {
                    old.cancel();
                }
                *guard = Some(control.clone());
            }
            if let Some(ui) = weak_main.upgrade() {
                ui.set_download_busy(true);
                ui.set_download_status("正在准备下载…".into());
            }
            if let Some(ui) = window.upgrade() {
                toast(&ui, "已开始下载", TOAST_KIND_SUCCESS);
            }
            download::spawn(
                session.clone(),
                bvid.clone(),
                cid,
                title.clone(),
                weak_main.clone(),
                control,
            );
        });
    }
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let window = weak.clone();
        let liked_flag = liked_flag.clone();
        player.on_toggle_like(move || {
            let target = !liked_flag.load(Ordering::SeqCst);
            let session = session.clone();
            let bvid = bvid.clone();
            let window = window.clone();
            let liked_flag = liked_flag.clone();
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                match runtime.block_on(session.like(&bvid, target)) {
                    Ok(()) => {
                        crate::debug_log(&format!("like set target={target} ok"));
                        liked_flag.store(target, Ordering::SeqCst);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(window) = window.upgrade() {
                                window.set_liked(target);
                                window.set_like_text(if target { "已赞" } else { "点赞" }.into());
                            }
                        });
                    }
                    Err(err) => {
                        crate::debug_log(&format!("like set target={target} err={err:#}"));
                        let message = format!("点赞失败：{err}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(window) = window.upgrade() {
                                toast(&window, message, TOAST_KIND_ERROR);
                            }
                        });
                    }
                }
            }));
        });
    }

    {
        let control = control.clone();
        let window = weak.clone();
        let state = state.clone();
        player.on_toggle_topmost(move || {
            let on = control.toggle_topmost();
            let mut applied = false;
            if let Some(window) = window.upgrade() {
                applied = win::set_topmost(window.window(), on);
                window.set_topmost(on);
                window.set_topmost_text(if on { "取消置顶" } else { "置顶" }.into());
            }
            if let Some(mini) = mini_handle(&state) {
                let _ = win::set_topmost(mini.window(), on);
            }
            if on {
                if let Some(player) = window.upgrade() {
                    ensure_topmost(true, Some(&player), &state);
                }
            }
            crate::debug_log(&format!("topmost on={on} applied={applied}"));
        });
    }
    {
        let control = control.clone();
        let state = state.clone();
        let window = weak.clone();
        // The event loop is the presentation thread: keep it above the prefetch
    // workers for the whole process (idempotent, UI thread only).
    player::raise_ui_thread_priority();
    let mini_slot = session_rec.mini_slot.clone();
        player.on_toggle_mini(move || {
            // The title strip is the truth: a 选集 switch may have moved this
            // window to another episode since it opened.
            let title = window
                .upgrade()
                .map(|window| window.get_video_title().to_string())
                .unwrap_or_default();
            toggle_mini(&state, &mini_slot, &window, &control, &title);
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
    // 音质: pick a DASH audio tier. Applies live (only `ffplay` restarts at the
    // current position); a tier this video does not carry is a toast, and the
    // selection stays where it was. The pick is persisted.
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        player.on_audio_quality_pick(move |id| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let request = i64::from(id);
            match control.set_audio_quality(request) {
                Some(index) => {
                    let rows = window.get_audio_quality_rows();
                    let updated: Vec<AudioQualityRow> = (0..rows.row_count())
                        .filter_map(|position| rows.row_data(position))
                        .map(|mut row| {
                            row.current = i64::from(row.id) == request;
                            row
                        })
                        .collect();
                    window.set_audio_quality_rows(ModelRc::from(std::rc::Rc::new(
                        VecModel::from(updated),
                    )));
                    window.set_audio_quality(id);
                    update_settings(&settings_store, |prefs| {
                        prefs.audio_quality = settings::normalise_audio_quality(id.max(0) as u32);
                    });
                    crate::debug_log(&format!(
                        "audio quality switch id={} index={} pos_ms={}",
                        request,
                        index,
                        control.playback_position_ms()
                    ));
                }
                None => {
                    let text = match request {
                        30250 => "杜比全景声需要大会员",
                        30251 => "Hi-Res 无损需要大会员",
                        _ => "该视频没有所选音质",
                    };
                    crate::toast(&window, text, TOAST_KIND_INFO);
                    crate::debug_log(&format!(
                        "audio quality denied id={} reason=absent locked=1",
                        request
                    ));
                }
            }
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_set_audio(move |index| {
            let index = index.max(0) as usize;
            control.set_audio_index(index);
            if let Some(window) = window.upgrade() {
                window.set_audio_index(index as i32);
                let items = window.get_audio_items();
                let mut label = window.get_audio_label();
                for position in 0..items.row_count() {
                    if let Some(entry) = items.row_data(position) {
                        if entry.index as usize == index {
                            label = entry.label;
                            break;
                        }
                    }
                }
                window.set_audio_label(label);
            }
        });
    }

    // Coin state on open, then the 投币 / 收藏 picker handlers.
    let coin_state = Arc::new(AtomicU64::new(0));
    // f179: the picker's rows are the shared truth between the network workers
    // and the UI handlers (fid, count, check state, whether it was confirmed).
    let fav_rows: Arc<Mutex<Vec<FavPick>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let window = weak.clone();
        let coin_state = coin_state.clone();
        crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
            let Some(runtime) = crate::bilibili::shared_runtime() else {
                return;
            };
            if let Ok(multiply) = runtime.block_on(session.coin_state(&bvid)) {
                let multiply = multiply.clamp(0, 2) as u64;
                coin_state.store(multiply, Ordering::SeqCst);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        window.set_coin_multiply(multiply as i32);
                        window.set_coin_text(
                            if multiply > 0 { "已投币" } else { "投币" }.into(),
                        );
                    }
                });
            }
        }));
    }
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let window = weak.clone();
        let coin_state = coin_state.clone();
        player.on_set_coin(move |multiply| {
            let multiply = i64::from(multiply).clamp(1, 2);
            let session = session.clone();
            let bvid = bvid.clone();
            let window = window.clone();
            let coin_state = coin_state.clone();
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.add_coin(&bvid, multiply, false));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok(()) => {
                                coin_state.store(multiply as u64, Ordering::SeqCst);
                                window.set_coin_multiply(multiply as i32);
                                window.set_coin_text("已投币".into());
                                toast(
                                    &window,
                                    format!("已投 {multiply} 枚硬币"),
                                    TOAST_KIND_SUCCESS,
                                );
                            }
                            Err(err) => {
                                crate::debug_log(&format!("add_coin err={err:#}"));
                                toast(
                                    &window,
                                    format!("投币失败：{err}"),
                                    TOAST_KIND_ERROR,
                                );
                            }
                        }
                    }
                });
            }));
        });
    }
    {
        let session = session.clone();
        let window = weak.clone();
        let fav_rows = fav_rows.clone();
        player.on_open_fav_menu(move || {
            if let Some(window) = window.upgrade() {
                window.set_fav_loading(true);
                window.set_fav_hint("".into());
                window.set_fav_hint_error(false);
            }
            let session = session.clone();
            let window = window.clone();
            let fav_rows = fav_rows.clone();
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                if aid <= 0 {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = window.upgrade() {
                            window.set_fav_loading(false);
                            window.set_fav_hint("该视频缺少 aid，无法读取收藏夹".into());
                            window.set_fav_hint_error(true);
                        }
                    });
                    return;
                }
                runtime.block_on(async move {
                    let folders = match session.fav_folders_all(aid).await {
                        Ok(folders) => folders,
                        Err(err) => {
                            crate::debug_log(&format!("fav picker folders err={err:#}"));
                            let text = format!("读取收藏夹失败：{err}");
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(window) = window.upgrade() {
                                    window.set_fav_loading(false);
                                    window.set_fav_hint(text.into());
                                    window.set_fav_hint_error(true);
                                }
                            });
                            return;
                        }
                    };
                    // The folder list reports this video's membership per folder
                    // (`fav_state`). The one-video endpoint is still asked: it
                    // is the authoritative "is it favourited at all / in how
                    // many folders" answer, and it is what makes an unresolved
                    // list honest instead of silently all-unchecked.
                    let (favoured, fav_count) =
                        session.fav_state(aid).await.unwrap_or((true, 0));
                    let mut picks: Vec<FavPick> = folders
                        .iter()
                        .map(|folder| {
                            let (checked, confirmed) = match folder.member {
                                Some(checked) => (checked, true),
                                None if !favoured => (false, true),
                                None => (false, false),
                            };
                            FavPick {
                                fid: folder.id,
                                title: folder.name.clone(),
                                count: folder.count,
                                checked,
                                confirmed,
                            }
                        })
                        .collect();
                    // The API says the video is in >= 1 folder, yet no row came
                    // back checked: the per-folder flags are not usable, so every
                    // row is marked 未确认 rather than drawn as "not in it".
                    if favoured && fav_count > 0 && !picks.iter().any(|row| row.checked) {
                        for row in picks.iter_mut() {
                            row.confirmed = false;
                        }
                        crate::debug_log(
                            "fav picker membership unresolved: favoured but no folder reported it",
                        );
                    }
                    crate::debug_log(&format!(
                        "fav picker rows={} checked={} unknown={} favoured={favoured}",
                        picks.len(),
                        picks.iter().filter(|row| row.checked).count(),
                        picks.iter().filter(|row| !row.confirmed).count()
                    ));
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = window.upgrade() {
                            let hint = if picks.is_empty() {
                                Some(("还没有收藏夹，可先新建一个", false))
                            } else {
                                None
                            };
                            publish_fav_picker(&window, &fav_rows, picks, hint);
                        }
                    });
                });
            }));
        });
    }
    {
        let session = session.clone();
        let window = weak.clone();
        let fav_rows = fav_rows.clone();
        player.on_fav_toggle(move |fid, add| {
            let Ok(folder_id) = fid.parse::<i64>() else {
                return;
            };
            if aid <= 0 {
                if let Some(window) = window.upgrade() {
                    toast(&window, "该视频缺少 aid，无法收藏", TOAST_KIND_ERROR);
                }
                return;
            }
            let before = fav_rows.lock().map(|guard| guard.clone()).unwrap_or_default();
            let title = before
                .iter()
                .find(|row| row.fid == folder_id)
                .map(|row| row.title.clone())
                .unwrap_or_default();
            // Optimistic: the check mark follows the click immediately; a
            // rejected request rolls it back to `before`.
            let mut optimistic = before.clone();
            for row in optimistic.iter_mut() {
                if row.fid == folder_id {
                    row.checked = add;
                    row.confirmed = true;
                }
            }
            if let Some(window) = window.upgrade() {
                publish_fav_picker(&window, &fav_rows, optimistic, None);
                window.set_fav_hint(
                    format!(
                        "{}「{title}」…",
                        if add { "正在加入" } else { "正在移出" }
                    )
                    .into(),
                );
                window.set_fav_hint_error(false);
            }
            crate::debug_log(&format!(
                "fav picker toggle rid={aid} folder={folder_id} add={add} title={title:?}"
            ));
            let session = session.clone();
            let window = window.clone();
            let fav_rows = fav_rows.clone();
            let before = before.clone();
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let (add_ids, del_ids): (Vec<i64>, Vec<i64>) = if add {
                    (vec![folder_id], Vec::new())
                } else {
                    (Vec::new(), vec![folder_id])
                };
                let result = runtime.block_on(session.fav_deal(aid, &add_ids, &del_ids));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(window) = window.upgrade() else {
                        return;
                    };
                    match result {
                        Ok(()) => {
                            crate::debug_log(&format!(
                                "fav deal {} ok rid={aid} folder={folder_id} title={title:?}",
                                if add { "add" } else { "del" }
                            ));
                            let mut rows = before.clone();
                            for row in rows.iter_mut() {
                                if row.fid == folder_id {
                                    row.checked = add;
                                    row.confirmed = true;
                                }
                            }
                            publish_fav_picker(
                                &window,
                                &fav_rows,
                                rows,
                                Some((
                                    &format!(
                                        "{}「{title}」",
                                        if add { "已加入" } else { "已移出" }
                                    ),
                                    false,
                                )),
                            );
                            toast(
                                &window,
                                if add {
                                    format!("已收藏到「{title}」")
                                } else {
                                    format!("已从「{title}」移出")
                                },
                                TOAST_KIND_SUCCESS,
                            );
                        }
                        Err(err) => {
                            crate::debug_log(&format!(
                                "fav picker deal err rid={aid} folder={folder_id} add={add} err={err:#}"
                            ));
                            publish_fav_picker(
                                &window,
                                &fav_rows,
                                before.clone(),
                                Some((&format!("收藏失败：{err}"), true)),
                            );
                            toast(&window, format!("收藏失败：{err}"), TOAST_KIND_ERROR);
                        }
                    }
                });
            }));
        });
    }
    {
        let session = session.clone();
        let window = weak.clone();
        let fav_rows = fav_rows.clone();
        player.on_fav_create(move |name| {
            let name = name.trim().to_string();
            if name.is_empty() {
                if let Some(window) = window.upgrade() {
                    window.set_fav_hint("收藏夹名称不能为空".into());
                    window.set_fav_hint_error(true);
                }
                return;
            }
            let session = session.clone();
            let window = window.clone();
            let fav_rows = fav_rows.clone();
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.fav_create(&name, 0));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(window) = window.upgrade() else {
                        return;
                    };
                    match result {
                        Ok(id) => {
                            let mut rows = fav_rows.lock().map(|guard| guard.clone()).unwrap_or_default();
                            rows.retain(|row| row.fid != id);
                            rows.push(FavPick {
                                fid: id,
                                title: name.clone(),
                                count: 0,
                                // A brand-new folder is empty, so "not in it"
                                // is a known fact, not a guess.
                                checked: false,
                                confirmed: true,
                            });
                            window.set_fav_new_name("".into());
                            publish_fav_picker(
                                &window,
                                &fav_rows,
                                rows,
                                Some((&format!("已新建收藏夹「{name}」"), false)),
                            );
                            toast(
                                &window,
                                format!("已新建收藏夹「{name}」"),
                                TOAST_KIND_SUCCESS,
                            );
                        }
                        Err(err) => {
                            crate::debug_log(&format!("fav picker create err name={name:?} err={err:#}"));
                            window.set_fav_hint(format!("新建收藏夹失败：{err}").into());
                            window.set_fav_hint_error(true);
                            toast(
                                &window,
                                format!("新建收藏夹失败：{err}"),
                                TOAST_KIND_ERROR,
                            );
                        }
                    }
                });
            }));
        });
    }
    {
        let session = session.clone();
        let window = weak.clone();
        let fav_rows = fav_rows.clone();
        player.on_unfav_all(move || {
            if aid <= 0 {
                if let Some(window) = window.upgrade() {
                    toast(&window, "该视频缺少 aid，无法取消收藏", TOAST_KIND_ERROR);
                }
                return;
            }
            let before = fav_rows.lock().map(|guard| guard.clone()).unwrap_or_default();
            let mut ids: Vec<i64> = before
                .iter()
                .filter(|row| row.checked)
                .map(|row| row.fid)
                .collect();
            if ids.is_empty() {
                ids = before.iter().map(|row| row.fid).collect();
            }
            crate::debug_log(&format!("fav picker unfav_all rid={aid} folders={ids:?}"));
            let session = session.clone();
            let window = window.clone();
            let fav_rows = fav_rows.clone();
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.fav_deal(aid, &[], &ids));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(window) = window.upgrade() else {
                        return;
                    };
                    match result {
                        Ok(()) => {
                            let mut rows = before.clone();
                            for row in rows.iter_mut() {
                                row.checked = false;
                                row.confirmed = true;
                            }
                            publish_fav_picker(
                                &window,
                                &fav_rows,
                                rows,
                                Some(("已取消收藏（全部收藏夹）", false)),
                            );
                            toast(&window, "已取消收藏", TOAST_KIND_SUCCESS);
                        }
                        Err(err) => {
                            crate::debug_log(&format!("fav picker unfav_all err={err:#}"));
                            publish_fav_picker(
                                &window,
                                &fav_rows,
                                before.clone(),
                                Some((&format!("取消收藏失败：{err}"), true)),
                            );
                            toast(
                                &window,
                                format!("取消收藏失败：{err}"),
                                TOAST_KIND_ERROR,
                            );
                        }
                    }
                });
            }));
        });
    }
    // 历史弹幕: fetch the available dates, then load one day's danmaku.
    {
        let session = session.clone();
        let window = weak.clone();
        player.on_open_history(move || {
            let session = session.clone();
            let window = window.clone();
            if let Some(window) = window.upgrade() {
                window.set_history_status("正在获取历史弹幕日期…".into());
            }
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.danmaku_history_dates(cid));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok(dates) => {
                                let count = dates.len();
                                let rows: Vec<SharedString> = dates
                                    .into_iter()
                                    .map(SharedString::from)
                                    .collect();
                                window.set_history_dates(ModelRc::from(std::rc::Rc::new(
                                    VecModel::from(rows),
                                )));
                                window.set_history_status(format!("{count} 个可用日期").into());
                            }
                            Err(err) => {
                                crate::debug_log(&format!("history dates err={err:#}"));
                                window.set_history_status(format!("获取日期失败：{err}").into());
                            }
                        }
                    }
                });
            }));
        });
    }
    {
        let session = session.clone();
        let window = weak.clone();
        let track = track.clone();
        let control = control.clone();
        player.on_set_history_date(move |date| {
            let date = date.to_string();
            let session = session.clone();
            let window = window.clone();
            let track = track.clone();
            let control = control.clone();
            if let Some(window) = window.upgrade() {
                window.set_history_status(format!("正在加载 {date} 的弹幕…").into());
            }
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let duration_ms = control.duration_ms();
                let result = runtime.block_on(session.danmaku_history(cid, &date));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok(history) => {
                                let count = history.items.len();
                                let (density, _, _, _) =
                                    player::compute_density(&history.items, duration_ms);
                                let rows = player::build_danmaku_rows(&history.items);
                                track.set_items(history.items);
                                window.set_density_buckets(ModelRc::from(std::rc::Rc::new(
                                    VecModel::from(density),
                                )));
                                window.set_danmaku_list(ModelRc::from(std::rc::Rc::new(
                                    VecModel::from(rows),
                                )));
                                window.set_danmaku_list_count(count as i32);
                                window.set_history_status(
                                    format!("{date}：{count} 条（{}）", history.source).into(),
                                );
                                // The bottom strip is gone, so nothing is opened
                                // here; `danmaku_list_open` stays inert. The
                                // dates/rows stay published for the settings
                                // panel's history list.
                            }
                            Err(err) => {
                                crate::debug_log(&format!("history date={date} err={err:#}"));
                                window.set_history_status(format!("加载失败：{err}").into());
                            }
                        }
                    }
                });
            }));
        });
    }

    // 发送弹幕: POST the current progress with a scroll-mode white comment.
    {
        let session = session.clone();
        let window = weak.clone();
        let control = control.clone();
        let refresh = refresh.clone();
        player.on_send_danmaku(move |text| {
            let text = text.to_string();
            if text.trim().is_empty() {
                if let Some(window) = window.upgrade() {
                    window.set_send_status("请输入弹幕内容".into());
                }
                return;
            }
            refresh.set(true);
            let progress = control.playback_position_ms();
            let session = session.clone();
            let window = window.clone();
            let bvid = bvid.clone();
            if let Some(window) = window.upgrade() {
                window.set_send_status("正在发送…".into());
            }
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.danmaku_post(
                    cid,
                    &bvid,
                    &text,
                    progress,
                    1,
                    25,
                    16_777_215,
                    0,
                ));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok((code, message)) => {
                                if code == 0 {
                                    window.set_send_status("发送成功 code=0".into());
                                    window.set_danmaku_input(String::new().into());
                                } else {
                                    window.set_send_status(
                                        format!("发送被拒绝：code={code} {message}").into(),
                                    );
                                }
                            }
                            Err(err) => {
                                crate::debug_log(&format!("dm/post err={err:#}"));
                                window.set_send_status(format!("发送失败：{err}").into());
                            }
                        }
                    }
                });
            }));
        });
    }

    // 弹幕举报: reason 11 (其他) with a short note.
    {
        let session = session.clone();
        let window = weak.clone();
        player.on_report_danmaku(move |dmid| {
            let dmid = dmid.to_string();
            if dmid.is_empty() {
                if let Some(window) = window.upgrade() {
                    toast(&window, "该弹幕缺少 dmid，无法举报", TOAST_KIND_ERROR);
                }
                return;
            }
            let session = session.clone();
            let window = window.clone();
            if let Some(window) = window.upgrade() {
                toast(&window, format!("正在举报 dmid={dmid}…"), TOAST_KIND_INFO);
            }
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.danmaku_report(cid, &dmid, 11, "客户端举报"));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok((code, message)) => {
                                crate::debug_log(&format!(
                                    "dm/report code={code} message={message}"
                                ));
                                let (text, kind) = if code == 0 {
                                    ("举报已提交".to_string(), TOAST_KIND_SUCCESS)
                                } else {
                                    (
                                        format!("举报被拒绝：code={code} {message}"),
                                        TOAST_KIND_ERROR,
                                    )
                                };
                                toast(&window, text, kind);
                            }
                            Err(err) => {
                                crate::debug_log(&format!("dm/report err={err:#}"));
                                toast(
                                    &window,
                                    format!("举报失败：{err}"),
                                    TOAST_KIND_ERROR,
                                );
                            }
                        }
                    }
                });
            }));
        });
    }

    // 弹幕点赞: toggle op 1 / 2 based on the row's current state.
    {
        let session = session.clone();
        let window = weak.clone();
        player.on_thumbup_danmaku(move |dmid| {
            let dmid = dmid.to_string();
            if dmid.is_empty() {
                return;
            }
            let Some(ui) = window.upgrade() else {
                return;
            };
            let items = ui.get_danmaku_list();
            let mut thumbed = false;
            let mut index = None;
            for position in 0..items.row_count() {
                if let Some(row) = items.row_data(position) {
                    if row.dmid.as_str() == dmid.as_str() {
                        thumbed = row.thumbed;
                        index = Some(position);
                        break;
                    }
                }
            }
            let op = if thumbed { 2 } else { 1 };
            let target = !thumbed;
            toast(
                &ui,
                format!("正在{}弹幕…", if target { "赞" } else { "取消赞" }),
                TOAST_KIND_INFO,
            );
            let session = session.clone();
            let window = window.clone();
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.danmaku_thumbup(cid, &dmid, op));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = window.upgrade() {
                        match result {
                            Ok((code, message)) => {
                                crate::debug_log(&format!(
                                    "dm/thumbup code={code} message={message}"
                                ));
                                if code == 0 {
                                    if let Some(position) = index {
                                        let model = ui.get_danmaku_list();
                                        let mut rows: Vec<DanmakuRow> = (0..model.row_count())
                                            .filter_map(|p| model.row_data(p))
                                            .collect();
                                        if let Some(row) = rows.get_mut(position) {
                                            row.thumbed = target;
                                        }
                                        ui.set_danmaku_list(ModelRc::from(std::rc::Rc::new(
                                            VecModel::from(rows),
                                        )));
                                    }
                                }
                                let (text, kind) = if code == 0 {
                                    if target {
                                        ("弹幕点赞成功".to_string(), TOAST_KIND_SUCCESS)
                                    } else {
                                        ("已取消弹幕点赞".to_string(), TOAST_KIND_SUCCESS)
                                    }
                                } else {
                                    (
                                        format!("弹幕点赞被拒绝：code={code} {message}"),
                                        TOAST_KIND_ERROR,
                                    )
                                };
                                toast(&ui, text, kind);
                            }
                            Err(err) => {
                                crate::debug_log(&format!("dm/thumbup err={err:#}"));
                                toast(
                                    &ui,
                                    format!("弹幕点赞失败：{err}"),
                                    TOAST_KIND_ERROR,
                                );
                            }
                        }
                    }
                });
            }));
        });
    }

    {
        let state = state.clone();
        let window = weak.clone();
        let record = Rc::downgrade(&session_rec);
        let store = settings_store.clone();
        let loops_keepalive = loops_slot.clone();
        player.on_player_closed(move || {
            close_player_window(&state, &window, &record, &store, "button");
            // The window's own close path also owns its pointer watch, even if
            // the session was already unregistered.
            if let Some(loops) = loops_keepalive.borrow_mut().take() {
                stop_player_loops(&loops);
            }
        });
    }
    {
        let state = state.clone();
        let window = weak.clone();
        let record = Rc::downgrade(&session_rec);
        let store = settings_store.clone();
        let loops_keepalive = loops_slot.clone();
        player.window().on_close_requested(move || {
            close_player_window(&state, &window, &record, &store, "native");
            if let Some(loops) = loops_keepalive.borrow_mut().take() {
                stop_player_loops(&loops);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    player::set_active_control(Some(control.clone()));
    // This window's 60Hz pointer watch for the chrome (`pointer_inside`) and
    // the debounced layout persist. One per window: the loop is stored in the
    // session AND captured by this window's tick/close callbacks, so a second
    // player always gets its own watch. Stopped by this window's close path.
    let loops = start_player_loops(&state, &session_rec, &settings_store);
    *session_rec.loops.borrow_mut() = Some(loops.clone());
    *loops_slot.borrow_mut() = Some(loops);
    // QA hook (`BILI_PLAYER_CLOSE_TEST=<ms>`): close the player through the
    // REAL ✕ callback after `ms`, so the close timeline can be read from
    // `BILI_DEBUG_LOG` without synthetic input. `native` instead posts
    // `WM_CLOSE`, which drives the window's own close-request path.
    if let Ok(value) = std::env::var("BILI_PLAYER_CLOSE_TEST") {
        let value = value.trim().to_string();
        let closer = weak.clone();
        std::thread::spawn(move || {
            let (delay, native) = match value.as_str() {
                "native" => (3000u64, true),
                other => (other.parse::<u64>().unwrap_or(3000), false),
            };
            std::thread::sleep(Duration::from_millis(delay));
            let _ = slint::invoke_from_event_loop(move || {
                let Some(player) = closer.upgrade() else {
                    return;
                };
                if native {
                    if let Some(hwnd) = win::hwnd_of(player.window()) {
                        crate::debug_log(&format!("player close test post WM_CLOSE hwnd={hwnd}"));
                        win::post_close(hwnd);
                    }
                } else {
                    crate::debug_log("player close test invoke ✕ callback");
                    player.invoke_player_closed();
                }
            });
        });
    }
    // QA hook: prove the outward-panel geometry numerically (no synthetic
    // input). See `panel_test_start`.
    if let Some((left, right, bottom, gap_ms)) = panel_test_spec() {
        panel_test_start(
            weak.clone(),
            panel_geom.clone(),
            left,
            right,
            bottom,
            gap_ms,
        );
    }
    // QA hook (`BILI_RESIZE_TEST=<ms>`): drive the REAL self-driven resize
    // path with explicit rects (`update_to_rect`, so it is cursor-independent
    // and never touches the pointer) while a video plays, and log
    // `fps_presented` once a second. If the OS modal resize loop were still in
    // play the pacer would stop for the whole gesture; instead the fps must
    // stay at the stream rate and the position must keep advancing.
    if let Ok(value) = std::env::var("BILI_RESIZE_TEST") {
        let duration = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(4000)
            .clamp(500, 30_000);
        run_resize_test(
            weak.clone(),
            player_resize.clone(),
            Duration::from_millis(duration),
            60,
        );
    }
    // QA hook (`BILI_TOAST_TEST=<delay_ms>`): invoke the real 截图 callback once
    // after `delay_ms` (default 2500), so the toast publish/expiry sequence can
    // be read from `BILI_DEBUG_LOG` without synthetic input.
    if let Ok(value) = std::env::var("BILI_TOAST_TEST") {
        let delay = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(2500)
            .clamp(500, 20_000);
        let weak = weak.clone();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            if let Some(player) = weak.upgrade() {
                crate::debug_log(&format!("toast-test invoke screenshot delay_ms={delay}"));
                player.invoke_take_screenshot();
            }
        });
    }
    // QA hook (`BILI_SCRUB_TEST=<delay_ms>`): select the whole-video scrub level
    // through the same property + callback the UI uses, then return to level 0
    // 1.5s later. Logs the published 20-cell heat and the clear, proving the
    // window/normalisation without synthetic input.
    if let Ok(value) = std::env::var("BILI_SCRUB_TEST") {
        let delay = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(8000)
            .clamp(500, 60_000);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let first = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(player) = first.upgrade() else {
                    return;
                };
                crate::debug_log(&format!(
                    "scrub-test open level=1 density_buckets={} duration_ms={:.0}",
                    player.get_density_buckets().row_count(),
                    player.get_duration_ms()
                ));
                player.set_scrub_level(1);
                player.invoke_scrub_level_changed(1);
            });
            std::thread::sleep(Duration::from_millis(1500));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    crate::debug_log("scrub-test close level=0");
                    player.set_scrub_level(0);
                    player.invoke_scrub_level_changed(0);
                }
            });
        });
    }
    // QA hook (`BILI_SEEK_DRAG_SHOT=<tag>[,<delay_ms>]`): replay ONE 2 s seek
    // gesture at pointer rate on the real player window - the same property
    // writes the TouchArea's move handler does (`dragging` + `drag_ms`, never a
    // seek) - capture the preview mid-drag, then release exactly as the Up
    // branch does (clear the flag, one `seek()`). The `seek requested` counter
    // is sampled before and after: the delta over the gesture must be 1.
    // No OS mouse event is synthesised.
    if let Ok(spec) = std::env::var("BILI_SEEK_DRAG_SHOT") {
        let mut parts = spec.split(',');
        let tag = parts
            .next()
            .map(|part| part.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .unwrap_or_else(|| "seek_drag".to_string());
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(8_000)
            .clamp(500, 120_000);
        let control = control.clone();
        let weak = weak.clone();
        std::thread::spawn(move || {
            for _ in 0..180 {
                if control.duration_ms() > 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            std::thread::sleep(Duration::from_millis(delay));
            let _ = slint::invoke_from_event_loop(move || {
                let Some(player) = weak.upgrade() else {
                    return;
                };
                let duration = player.get_duration_ms().max(1.0f32);
                let start = duration * 0.12f32;
                let end = duration * 0.56f32;
                // Hold the chrome up for the capture; the pointer watch keeps
                // its own last real value and gets it back when the drag ends.
                player.set_pointer_inside(true);
                let seeks_before = SEEK_REQUEST_COUNT.load(Ordering::SeqCst);
                let path = qa_capture_path(&tag);
                let timer = Rc::new(slint::Timer::default());
    let timer_a = timer.clone();
                keep_qa_timer(timer.clone());
                let tick = Rc::new(Cell::new(0u32));
                            crate::debug_log(&format!(
                    "seek-drag-test start duration_ms={duration:.0} from_ms={start:.0} to_ms={end:.0} seeks_before={seeks_before}"
                ));
                // Hold the "button is down" state for the length of the
                // replay, exactly as a real press does.
                player.set_qa_drag_hold(true);
                player.set_dragging(true);
                player.set_drag_ms(start as f32);
                timer.start(
                    slint::TimerMode::Repeated,
                    Duration::from_millis(16),
                    move || {
                        let n = tick.get();
                        if n < 125 {
                            // 125 ticks x 16ms = 2.0s of pointer-rate preview.
                            let at = start + (end - start) * (n as f32) / 125.0;
                            player.set_drag_ms(at as f32);
                            if n == 60 {
                                crate::debug_log(&format!(
                                    "seek-drag-test preview preview_ms={at:.0} position_ms={:.0} dragging={} seeks_now={}",
                                    player.get_position_ms(),
                                    player.get_dragging(),
                                    SEEK_REQUEST_COUNT.load(Ordering::SeqCst)
                                ));
                                let _ = capture_window_png(&player.window(), &path);
                            }
                            tick.set(n + 1);
                        } else {
                            // Release: the same writes as the Up branch
                            // (`apply-seek`): publish the target to the shown
                            // position/fill/readout, then issue the seek.
                            player.set_drag_ms(end as f32);
                            player.set_dragging(false);
                            player.set_qa_drag_hold(false);
                            player.set_scrub_settle_ms(end as f32);
                            player.set_scrub_settling(true);
                            let seeks_after_drag = SEEK_REQUEST_COUNT.load(Ordering::SeqCst);
                            player.invoke_seek(end as f32);
                            let seeks_after = SEEK_REQUEST_COUNT.load(Ordering::SeqCst);
                            crate::debug_log(&format!(
                                "seek-drag-test release at_ms={end:.0} seeks_during_drag={} seeks_after_release={} delta={}",
                                seeks_after_drag - seeks_before,
                                seeks_after - seeks_before,
                                seeks_after - seeks_before
                            ));
                            player.set_pointer_inside(player_pointer_inside(&player));
                            timer_a.stop();
                        }
                    },
                );
            });
        });
    }
    // QA hooks (`BILI_F187_FAV_SHOT` / `BILI_F187_PREVIEW_SHOT`): the two
    // player-window captures this round needs (f179 picker, f185 hover
    // preview). Both drive the SAME state the UI writes - no synthetic pointer
    // event, no test-only rendering path - and hand the chrome back afterwards.
    //
    //   BILI_F187_FAV_SHOT=<tag>[,<delay_ms>[,<toggle>]]
    //     收藏 picker: open the right panel on 信息 (its home), run the chip's
    //     own `open_fav_menu` callback, force the expanded state with the QA
    //     seam, wait for the folder rows, capture, release. `toggle=1` also
    //     proves a real add: the row with the fewest videos is clicked through
    //     `fav_toggle`, the capture is taken with the check mark and the
    //     「已加入…」 hint up, and the SAME row is clicked again right after, so
    //     the account is left exactly as it was found.
    //   BILI_F187_PREVIEW_SHOT=<tag>[,<delay_ms>[,<frac>[,<level>]]]
    //     悬停预览: wait for `sb_ready` (the storyboard atlas), park the fake
    //     cursor at `frac` of the seek band (level 0, the normal bar) or of the
    //     zoom strip (level 1, the right-click scrubber), capture, release.
    if let Ok(spec) = std::env::var("BILI_F187_FAV_SHOT") {
        let mut parts = spec.split(',');
        let tag = parts
            .next()
            .map(|part| part.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .unwrap_or_else(|| "f187_fav_picker".to_string());
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(9_000)
            .clamp(1_000, 120_000);
        let toggle = parts
            .next()
            .map(|part| part.trim() == "1")
            .unwrap_or(false);
        let weak = weak.clone();
        let timer = Rc::new(slint::Timer::default());
        let stop_timer = timer.clone();
        keep_qa_timer(timer.clone());
        let step = Rc::new(Cell::new(0u8));
        let tries = Rc::new(Cell::new(0u32));
        let waited = Rc::new(Cell::new(0u64));
        // The folder this run adds to (and then removes from again).
        let target = Rc::new(Cell::new(0i64));
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(500),
            move || {
                let Some(player) = weak.upgrade() else {
                    return;
                };
                let n = tries.get() + 1;
                tries.set(n);
                let waited_ms = waited.get() + 500;
                waited.set(waited_ms);
                match step.get() {
                    0 => {
                        // `delay` first (let the decoder settle), then the
                        // first frame: the panel and the toolbar only exist
                        // once the player has a picture.
                        if waited_ms < delay || (player.get_duration_ms() <= 0.0 && n < 120) {
                            return;
                        }
                        player.set_pointer_inside(true);
                        player.set_right_panel(1);
                        player.set_right_tab(0);
                        player.set_qa_fav_open(true);
                        player.invoke_open_fav_menu();
                        crate::debug_log(&format!(
                            "f187 fav shot panel=1 tab=0 delay={delay} rows={} toggle={}",
                            player.get_fav_items().row_count(),
                            u8::from(toggle)
                        ));
                        step.set(1);
                    }
                    1 => {
                        // The list arrives on the netpool; give it a moment.
                        let rows = player.get_fav_items().row_count();
                        if (rows == 0 && player.get_fav_loading()) && n < 120 {
                            return;
                        }
                        crate::debug_log(&format!(
                            "f187 fav shot rows={rows} loading={} hint={:?}",
                            player.get_fav_loading(),
                            player.get_fav_hint()
                        ));
                        step.set(2);
                    }
                    2 => {
                        if !toggle {
                            step.set(4);
                            return;
                        }
                        // The lightest unchecked folder: one video added and
                        // immediately removed leaves the account as it was
                        // found (no folder keeps a stray entry).
                        let model = player.get_fav_items();
                        let mut pick: Option<(i32, i64, i32)> = None;
                        for i in 0..model.row_count() {
                            let Some(row) = model.row_data(i as usize) else {
                                continue;
                            };
                            if row.checked {
                                continue;
                            }
                            let fid = row.fid.to_string().parse::<i64>().unwrap_or(0);
                            if fid == 0 {
                                continue;
                            }
                            let better = match pick {
                                None => true,
                                Some((_, _, count)) => row.count < count,
                            };
                            if better {
                                pick = Some((i as i32, fid, row.count));
                            }
                        }                        match pick {
                            Some((_, fid, count)) => {
                                target.set(fid);
                                crate::debug_log(&format!(
                                    "f187 fav shot toggle add fid={fid} count={count}"
                                ));
                                player.invoke_fav_toggle(fid.to_string().into(), true);
                                step.set(3);
                            }
                            None => {
                                crate::debug_log(
                                    "f187 fav shot toggle skipped reason=no-unchecked-folder",
                                );
                                step.set(4);
                            }
                        }
                    }
                    3 => {
                        // Wait for the add to land: the hint is its receipt.
                        if player.get_fav_hint() == "" && n < 120 {
                            return;
                        }
                        let model = player.get_fav_items();
                        let checked = (0..model.row_count())
                            .filter(|i| {
                                model
                                    .row_data(*i as usize)
                                    .is_some_and(|row| row.checked)
                            })
                            .count();
                        crate::debug_log(&format!(
                            "f187 fav shot add hint={:?} checked={checked}",
                            player.get_fav_hint()
                        ));
                        step.set(4);
                    }
                    4 => {
                        let path = qa_capture_path(&tag);
                        match capture_window_png(&player.window(), &path) {
                            Some((w, h)) => crate::debug_log(&format!(
                                "f187 fav shot png={} {w}x{h}",
                                path.display()
                            )),
                            None => crate::debug_log("f187 fav shot png=FAILED"),
                        }
                        let fid = target.get();
                        if toggle && fid != 0 {
                            // Put the account back exactly as it was found.
                            player.invoke_fav_toggle(fid.to_string().into(), false);
                            crate::debug_log(&format!("f187 fav shot toggle del fid={fid}"));
                            step.set(5);
                        } else {
                            step.set(6);
                        }
                    }
                    5 => {
                        if player.get_fav_hint() == "" && n < 240 {
                            return;
                        }
                        crate::debug_log(&format!(
                            "f187 fav shot restored hint={:?}",
                            player.get_fav_hint()
                        ));
                        step.set(6);
                    }
                    _ => {
                        player.set_qa_fav_open(false);
                        player.set_pointer_inside(player_pointer_inside(&player));
                        crate::debug_log("f187 fav shot chrome=release");
                        stop_timer.stop();
                    }
                }
            },
        );
    }
    if let Ok(spec) = std::env::var("BILI_F187_PREVIEW_SHOT") {
        let mut parts = spec.split(',');
        let tag = parts
            .next()
            .map(|part| part.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .unwrap_or_else(|| "f187_hover_preview".to_string());
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(9_000)
            .clamp(1_000, 120_000);
        let frac = parts
            .next()
            .and_then(|part| part.trim().parse::<f32>().ok())
            .unwrap_or(0.62)
            .clamp(0.05, 0.95);
        let level = parts
            .next()
            .and_then(|part| part.trim().parse::<i32>().ok())
            .unwrap_or(0)
            .clamp(0, 1);
        let weak = weak.clone();
        let timer = Rc::new(slint::Timer::default());
        let stop_timer = timer.clone();
        keep_qa_timer(timer.clone());
        let step = Rc::new(Cell::new(0u8));
        let tries = Rc::new(Cell::new(0u32));
        let waited = Rc::new(Cell::new(0u64));
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(500),
            move || {
                let Some(player) = weak.upgrade() else {
                    return;
                };
                let n = tries.get() + 1;
                tries.set(n);
                let waited_ms = waited.get() + 500;
                waited.set(waited_ms);
                match step.get() {
                    0 => {
                        if waited_ms < delay || (player.get_duration_ms() <= 0.0 && n < 120) {
                            return;
                        }
                        if !player.get_sb_ready() && n < 240 {
                            // Up to ~4 min for the atlas; log the outcome once.
                            if n % 20 == 0 {
                                crate::debug_log(&format!(
                                    "f187 preview wait sb_ready=0 tries={n}"
                                ));
                            }
                            return;
                        }
                        player.set_pointer_inside(true);
                        if level > 0 {
                            player.set_scrub_level(1);
                            player.invoke_scrub_level_changed(1);
                        }
                        let band = if level > 0 {
                            player.get_qa_pill_w() as f32
                        } else {
                            player.get_qa_seek_w() as f32
                        };
                        player.set_qa_preview_x(band * frac);
                        player.set_qa_preview_hover(true);
                        crate::debug_log(&format!(
                            "f187 preview sb_ready={} cells={} cols={} level={level} frac={frac} band={band:.0}",
                            player.get_sb_ready(),
                            player.get_sb_cells().row_count(),
                            player.get_sb_cols()
                        ));
                        step.set(1);
                    }
                    1 => {
                        let path = qa_capture_path(&tag);
                        match capture_window_png(&player.window(), &path) {
                            Some((w, h)) => crate::debug_log(&format!(
                                "f187 preview png={} {w}x{h} level={level}",
                                path.display()
                            )),
                            None => crate::debug_log("f187 preview png=FAILED"),
                        }
                        step.set(2);
                    }
                    _ => {
                        player.set_qa_preview_hover(false);
                        if level > 0 {
                            player.set_scrub_level(0);
                            player.invoke_scrub_level_changed(0);
                        }
                        player.set_pointer_inside(player_pointer_inside(&player));
                        crate::debug_log("f187 preview chrome=release");
                        stop_timer.stop();
                    }
                }
            },
        );
    }
    // QA hook (`BILI_QUALITY_PICK=<index>[,<delay_ms>]`): invoke the real 画质
    // menu callback after `delay_ms` (default 12000), so an exact-stream switch
    // can be read from `BILI_DEBUG_LOG` (`quality pick …` +
    // `stream … pick=exact`) without synthetic input.
    if let Ok(value) = std::env::var("BILI_QUALITY_PICK") {
        let mut parts = value.split(',');
        let index = parts
            .next()
            .and_then(|part| part.trim().parse::<i32>().ok())
            .filter(|index| *index >= 0);
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(12_000)
            .clamp(500, 120_000);
        if let Some(index) = index {
            let weak = weak.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(player) = weak.upgrade() else {
                        return;
                    };
                    crate::debug_log(&format!(
                        "quality-test invoke pick index={index} rows={}",
                        player.get_quality_menu_rows().row_count()
                    ));
                    player.invoke_quality_pick(index);
                });
            });
        }
    }
    // QA hook (`BILI_QUALITY_SWITCH_TEST=<qn>[,<qn>…]`): drive the real 画质
    // menu on a playing window, one tier every `BILI_QUALITY_SWITCH_MS`
    // (default 15 000 ms), cycling through the list.
    //
    // It is the reproduction for "play for a while, then switch quality and the
    // stream never loads": the switch goes through the same callback a click
    // fires (`invoke_quality_pick` → `on_quality_pick` → `set_quality_exact`
    // → decode restart), so the log train it produces is the shipped one:
    // `quality pick`, `stream qn=`, `video target reason=restart`, the prefetch
    // session's own lines and the new session's `session first present`.
    //
    // A `qn` that is not in the merged menu (a locked 大会员 tier, a tier the
    // video does not have) is logged and skipped, never forced.
    if let Some(value) = crate::qa::env::var("BILI_QUALITY_SWITCH_TEST") {
        let qns: Vec<i64> = value
            .split(',')
            .filter_map(|part| part.trim().parse::<i64>().ok())
            .filter(|qn| *qn > 0)
            .collect();
        let interval = crate::qa::env::var("BILI_QUALITY_SWITCH_MS")
            .and_then(|ms| ms.parse::<u64>().ok())
            .unwrap_or(15_000)
            .clamp(2_000, 120_000);
        if !qns.is_empty() {
            let weak = weak.clone();
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                crate::debug_log(&format!(
                    "quality-switch test start qns={qns:?} interval_ms={interval}"
                ));
                // Slint properties may only be read on the event-loop thread, so
                // the menu is inspected inside the invoke; a window that has not
                // published its rows yet (the playurl is still in flight) is
                // waited for instead of counting as a switch.
                let mut step = 0usize;
                loop {
                    std::thread::sleep(Duration::from_millis(interval));
                    let qn = qns[step % qns.len()];
                    let weak = weak.clone();
                    // `alive` + `Some(row)` = switched, `alive` + `None` = no such
                    // row (locked tier / not offered), `!alive` = window gone.
                    let (tx, rx) = std::sync::mpsc::channel::<(bool, Option<(i32, i64)>, usize)>();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(player) = weak.upgrade() else {
                            let _ = tx.send((false, None, 0));
                            return;
                        };
                        let rows = player.get_quality_menu_rows();
                        if rows.row_count() == 0 {
                            let _ = tx.send((true, None, 0));
                            return;
                        }
                        let mut found: Option<(i32, i64)> = None;
                        for index in 0..rows.row_count() {
                            let Some(row) = rows.row_data(index) else {
                                continue;
                            };
                            if row.qn as i64 == qn && !row.locked {
                                found = Some((index as i32, row.qn as i64));
                                break;
                            }
                        }
                        if let Some((index, _qn)) = found {
                            player.invoke_quality_pick(index);
                        }
                        let _ = tx.send((true, found, rows.row_count()));
                    });
                    match rx.recv_timeout(Duration::from_secs(5)) {
                        Ok((false, _, _)) => return,
                        Ok((true, None, 0)) => {
                            crate::debug_log(&format!(
                                "quality-switch wait qn={qn} reason=no-rows at_ms={}",
                                started.elapsed().as_millis()
                            ));
                            std::thread::sleep(Duration::from_millis(1_000));
                        }
                        Ok((true, None, rows)) => {
                            crate::debug_log(&format!(
                                "quality-switch skip qn={qn} reason=no-row rows={rows}"
                            ));
                            step += 1;
                        }
                        Ok((true, Some((index, qn)), _)) => {
                            step += 1;
                            crate::debug_log(&format!(
                                "quality-switch step={step} qn={qn} row={index} at_ms={}",
                                started.elapsed().as_millis()
                            ));
                        }
                        Err(_) => return,
                    }
                }
            });
        }
    }
    // QA hook (`BILI_QUALITY_MENU_TEST=1`): once the merged 画质 menu has rows,
    // open the 设置 tab and the 画质 menu through the same UI state the 画质 row
    // writes, then capture the real player window with `PrintWindow` after it
    // paints. No synthetic input; the capture path is logged.
    if std::env::var("BILI_QUALITY_MENU_TEST")
        .map(|value| value == "1")
        .unwrap_or(false)
    {
        // Capture tag (`BILI_QUALITY_MENU_TAG`, named by the QA run); the PNG
        // lands in %TEMP% as `bili_panel_test_<tag>_WxH.png`.
        let tag = std::env::var("BILI_QUALITY_MENU_TAG")
            .ok()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .unwrap_or_else(|| "quality_locked".to_string());
        let weak = weak.clone();
        let timer = Rc::new(slint::Timer::default());
    let timer_a = timer.clone();
        keep_qa_timer(timer.clone());
        let step = Rc::new(Cell::new(0u8));
            timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(800),
            move || {
                let Some(player) = weak.upgrade() else {
                    timer_a.stop();
                    return;
                };
                match step.get() {
                    0 => {
                        let rows = player.get_quality_menu_rows().row_count();
                        if rows == 0 {
                            return;
                        }
                        // The menu is a chrome child: `ui/app.slint` closes it
                        // while `pointer_inside` is false. Hold the chrome up
                        // for the capture only - the pointer watch keeps its own
                        // last real value, so putting it back below resumes the
                        // normal cursor-driven behaviour. No pointer event is
                        // synthesised.
                        player.set_pointer_inside(true);
                        player.set_right_panel(1);
                        player.set_right_tab(3);
                        crate::debug_log(&format!(
                            "quality-menu-test panel open rows={rows} chrome=hold"
                        ));
                        step.set(1);
                    }
                    1 => {
                        player.invoke_open_quality_menu();
                        crate::debug_log("quality-menu-test menu open");
                        step.set(2);
                    }
                    2 => {
                        step.set(3);
                        let _ = capture_player_png(&player.window(), &tag);
                        crate::debug_log(&format!("quality-menu-test captured tag={tag}"));
                    }
                    3 => {
                        // Hand the chrome back to the real cursor position.
                        player.set_pointer_inside(player_pointer_inside(&player));
                        crate::debug_log("quality-menu-test chrome=release");
                        timer_a.stop();
                    }
                    _ => timer_a.stop(),
                }
            },
        );
    }
    // QA hook (`BILI_COMMENTS_MORE_TEST=<delay_ms>[,<count>]`): call the real
    // `comments_more()` callback, so the page-ahead buffer is provable from
    // `BILI_DEBUG_LOG` (`comments prefetch …` then `comments more from
    // prefetch …`) without synthetic input.
    if let Ok(value) = std::env::var("BILI_COMMENTS_MORE_TEST") {
        let mut parts = value.split(',');
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(6_000)
            .clamp(500, 120_000);
        let count = parts
            .next()
            .and_then(|part| part.trim().parse::<u32>().ok())
            .unwrap_or(2)
            .clamp(1, 10);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            for step in 0..count {
                let weak_step = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(player) = weak_step.upgrade() else {
                        return;
                    };
                    crate::debug_log(&format!(
                        "comments-more-test step={step} rows_before={} loading={} end={}",
                        player.get_comment_items().row_count(),
                        player.get_comments_loading(),
                        player.get_comments_end()
                    ));
                    player.invoke_comments_more();
                });
                std::thread::sleep(Duration::from_millis(600));
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    crate::debug_log(&format!(
                        "comments-more-test done rows={} loading={} end={}",
                        player.get_comment_items().row_count(),
                        player.get_comments_loading(),
                        player.get_comments_end()
                    ));
                }
            });
        });
    }
    // QA hook (`BILI_PLAYER_CHROME_TEST=<delay_ms>[,<tab>]`): open the player's
    // right panel on the requested tab (default 信息) and leave it there, in
    // windowed mode. BILI_PANEL_GLASS_TEST does the fullscreen dance; this one
    // is the steady state the f133 chrome capture needs.
    if let Ok(value) = std::env::var("BILI_PLAYER_CHROME_TEST") {
        let mut parts = value.split(',');
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(9_000)
            .clamp(2_000, 120_000);
        let tab = parts
            .next()
            .and_then(|part| part.trim().parse::<i32>().ok())
            .unwrap_or(0)
            .clamp(0, 3);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    player.set_right_panel(1);
                    player.set_right_tab(tab);
                    player.invoke_panel_changed();
                    log_panel_state(&player, "f133-chrome");
                }
            });
        });
    }
    // QA hook (`BILI_PANEL_GLASS_TEST=<delay_ms>[,<hold_ms>]`): open the right
    // panel, enter fullscreen through the REAL toggle, hold, then leave again.
    // It is the only way to reach the panel/fullscreen layout without synthetic
    // input, so the panel and letterbox captures are reproducible; the name is
    // kept for the existing scripts.
    if let Ok(value) = std::env::var("BILI_PANEL_GLASS_TEST") {
        let mut parts = value.split(',');
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(8_000)
            .clamp(2_000, 60_000);
        let hold = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(4_000)
            .clamp(1_000, 60_000);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(player) = weak.upgrade() {
                        player.set_right_panel(1);
                        player.set_right_tab(2);
                        player.invoke_panel_changed();
                        log_panel_state(&player, "panel-open");
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(900));
            {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(player) = weak.upgrade() {
                        player.invoke_toggle_fullscreen();
                        log_panel_state(&player, "fullscreen");
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(1_600));
            {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(player) = weak.upgrade() {
                        log_panel_state(&player, "overlay");
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(1_500 + hold));
            {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(player) = weak.upgrade() {
                        player.invoke_toggle_fullscreen();
                        log_panel_state(&player, "exit");
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(1_600));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    log_panel_state(&player, "windowed");
                }
            });
        });
    }
    // Diagnostic hook (mirrors BILI_AUTOPLAY / BILI_MINI): seek to BILI_SEEK_TEST
    // once playback is ready, logging the position before and after, so a seek
    // landing can be read from BILI_DEBUG_LOG without synthetic input.
    // `<seconds>` is an absolute offset; `p<N>` seeks to N% of the duration
    // (always inside the track, so a deep seek works on any test video).
    // `BILI_SEEK_DELAY_MS` (default 15000) shifts the seek, so a run can land
    // after the background seek-point warm has finished.
    if let Ok(spec) = std::env::var("BILI_SEEK_TEST") {
        let spec = spec.trim().to_string();
        if !spec.is_empty() {
            let control = control.clone();
            let seek_delay = std::env::var("BILI_SEEK_DELAY_MS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(15_000)
                .clamp(1_000, 600_000);
            std::thread::spawn(move || {
                for _ in 0..120 {
                    if control.duration_ms() > 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                std::thread::sleep(Duration::from_millis(seek_delay));
                let before = control.playback_position_ms();
                let duration = control.duration_ms();
                let target = if let Some(percent) = spec.strip_prefix('p') {
                    let percent = percent.parse::<u64>().unwrap_or(95).clamp(1, 99);
                    duration.saturating_mul(percent) / 100
                } else {
                    spec.parse::<u64>().unwrap_or(0).saturating_mul(1000)
                };
                let target = if duration > 0 {
                    target.min(duration.saturating_sub(1_000))
                } else {
                    target
                };
                crate::debug_log(&format!(
                    "seek-test spec={spec} before_ms={before} target_ms={target} duration_ms={duration}"
                ));
                control.seek(target);
                std::thread::sleep(Duration::from_millis(1_500));
                let after = control.playback_position_ms();
                crate::debug_log(&format!(
                    "seek-test after_ms={after} target_ms={target} delta_ms={}",
                    after as i64 - target as i64
                ));
                // End→seek check (no synthetic input): wait for the track to
                // reach its end, then seek to BILI_SEEK_AGAIN and prove the
                // decoder restarted and the position advances again.
                if let Ok(again) = std::env::var("BILI_SEEK_AGAIN") {
                    let again = again.trim().to_string();
                    if !again.is_empty() {
                        let mut ended = false;
                        // A seek near EOF can take a while on a slow CDN; give
                        // it up to a minute to reach the real end.
                        for _ in 0..120 {
                            if control.track_ended() {
                                ended = true;
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(500));
                        }
                        let duration = control.duration_ms();
                        let target = if let Some(percent) = again.strip_prefix('p') {
                            let percent = percent.parse::<u64>().unwrap_or(10).clamp(1, 99);
                            duration.saturating_mul(percent) / 100
                        } else {
                            again.parse::<u64>().unwrap_or(10).saturating_mul(1_000)
                        };
                        let target = if duration > 0 {
                            target.min(duration.saturating_sub(1_000))
                        } else {
                            target
                        };
                        let before = control.playback_position_ms();
                        crate::debug_log(&format!(
                            "seek-again spec={again} ended={ended} before_ms={before} target_ms={target} duration_ms={duration}"
                        ));
                        control.seek(target);
                        // A far seek can spend a second or two buffering; wait
                        // past that so "the position advances" is measured on a
                        // running picture, not on the held seek target.
                        std::thread::sleep(Duration::from_millis(4_500));
                        let after = control.playback_position_ms();
                        crate::debug_log(&format!(
                            "seek-again after_ms={after} target_ms={target} advanced_ms={}",
                            after as i64 - target as i64
                        ));
                    }
                }
            });
        }
    }
    // Diagnostic hook (mirrors BILI_RESIZE_TEST): toggle fullscreen on and back
    // off through the same UI callback the button uses, so the decode target's
    // window-independence can be read from the backend without synthetic input.
    // `BILI_FULLSCREEN_TEST=<delay_ms>` (default 8000) is when the first toggle
    // happens; the second follows 4 s later.
    if let Ok(value) = std::env::var("BILI_FULLSCREEN_TEST") {
        let delay = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(8_000)
            .clamp(1_000, 60_000);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let first = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = first.upgrade() {
                    crate::debug_log(&format!("fullscreen-test toggle=on delay_ms={delay}"));
                    player.invoke_toggle_fullscreen();
                }
            });
            std::thread::sleep(Duration::from_secs(4));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    crate::debug_log("fullscreen-test toggle=off");
                    player.invoke_toggle_fullscreen();
                }
            });
        });
    }
    // Diagnostic hook (mirrors BILI_AUTOPLAY): open the mini PiP immediately so
    // its frameless frame style can be read back from the backend.
    if std::env::var("BILI_MINI").map(|value| value == "1").unwrap_or(false) {
        toggle_mini(
            &state,
            &session_rec.mini_slot,
            &weak,
            &control,
            &title_for_mini,
        );
    }
    // ---- 评论区 (left sidebar) ----
    // The exact registration the old TODO asked for: the data layer's four entry
    // points plus the UI's row-expand (Rust owns `expanded`, so a republish keeps
    // the thread open). All of them clone `session`, which `player::spawn` takes
    // right below. The two comment callbacks carry a row INDEX (never an id):
    // the index -> rpid map is the one `publish_comments` wrote alongside the
    // model, so a rpid wider than 32 bits survives the round trip.
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comments_load(move || {
            on_comments_load(session.clone(), comments.clone(), aid, true);
        });
    }
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comments_more(move || {
            on_comments_more(session.clone(), comments.clone());
        });
    }
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comments_sort(move |hot| {
            on_comments_sort(session.clone(), comments.clone(), hot);
        });
    }
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comment_replies(move |row, pn| {
            on_comment_replies(
                session.clone(),
                comments.clone(),
                row,
                pn as i64,
            );
        });
    }
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comment_toggle(move |row| {
            on_comment_toggle(session.clone(), comments.clone(), row);
        });
    }

    // ---- 选集 (分P / 合集 / 番剧) ---------------------------------------------
    // The player's ONE list entry point. One `video_view` on THIS player's
    // session (the same call `BILI_OPEN_BVID` resolves with) fills a flat row
    // model; the toolbar cell is enabled by `series_kind` alone. A failed fetch
    // changes nothing (kind stays 0, the cell stays dim), so a single-P video,
    // a private video and a network error all read the same. 番剧 skips the
    // fetch: its season detail arrives with the open (`kind = 3`).
    let series_state: Arc<Mutex<SeriesState>> = Arc::new(Mutex::new(SeriesState::default()));
    {
        let weak = weak.clone();
        let series_state = series_state.clone();
        let control = control.clone();
        let session = session.clone();
        let comments = comments.clone();
        // The bvid this window opened with: 分P rows all live on it, and a
        // 合集 episode that omits its own bvid falls back to it.
        let open_bvid = video.bvid.clone();
        player.on_series_pick(move |index| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let index = index.max(0) as usize;
            let (target, kind) = {
                let state = match series_state.lock() {
                    Ok(state) => state,
                    Err(_) => return,
                };
                match state.targets.get(index).and_then(|entry| entry.clone()) {
                    Some(target) => (target, state.kind),
                    None => {
                        crate::debug_log(&format!("series pick ignored row={index} reason=header"));
                        return;
                    }
                }
            };
            if kind == 3 && target.ep_id > 0 {
                // 番剧: gate the episode first, so a region-locked / removed
                // episode shows B站's own reason instead of replacing the
                // picture with a failure the player can only log.
                let gate_session = session.clone();
                let gate_weak = window.as_weak();
                let gate_series = series_state.clone();
                let gate_control = control.clone();
                let gate_comments = comments.clone();
                let gate_bvid = open_bvid.clone();
                crate::netpool::submit(
                    crate::netpool::Prio::Normal,
                    Box::new(move || {
                        let Some(runtime) = crate::bilibili::shared_runtime() else {
                            return;
                        };
                        let gate = runtime.block_on(
                            gate_session.pgc_gate(target.ep_id, target.cid),
                        );
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(window) = gate_weak.upgrade() else {
                                return;
                            };
                            match gate {
                                Ok(gate) if gate.code != 0 => {
                                    let reason = crate::bilibili::pgc_reason(gate.code)
                                        .map(str::to_string)
                                        .unwrap_or_else(|| {
                                            if gate.message.trim().is_empty() {
                                                format!("B站返回 code={}", gate.code)
                                            } else {
                                                format!(
                                                    "{}（code={}）",
                                                    gate.message.trim(),
                                                    gate.code
                                                )
                                            }
                                        });
                                    crate::debug_log(&format!(
                                        "pgc series gate blocked ep={} cid={} code={} reason={reason}",
                                        target.ep_id, target.cid, gate.code
                                    ));
                                    toast(&window, reason, TOAST_KIND_ERROR);
                                }
                                Ok(_) => apply_series_pick(
                                    &window,
                                    &gate_series,
                                    &gate_control,
                                    &gate_comments,
                                    &gate_session,
                                    &gate_bvid,
                                    index,
                                    target,
                                    kind,
                                ),
                                Err(err) => {
                                    crate::debug_log(&format!(
                                        "pgc series gate err ep={} err={err:#}",
                                        target.ep_id
                                    ));
                                    apply_series_pick(
                                        &window,
                                        &gate_series,
                                        &gate_control,
                                        &gate_comments,
                                        &gate_session,
                                        &gate_bvid,
                                        index,
                                        target,
                                        kind,
                                    );
                                }
                            }
                        });
                    }),
                );
            } else {
                apply_series_pick(
                    &window,
                    &series_state,
                    &control,
                    &comments,
                    &session,
                    &open_bvid,
                    index,
                    target,
                    kind,
                );
            }
        });
    }

    if let Some(pgc) = pgc.clone() {
        // 番剧: the season detail is already in hand, so the whole 选集 list is
        // published synchronously - no second video_view, no empty panel frame.
        let state = series_from_pgc(&pgc.season, pgc.ep_id);
        crate::debug_log(&format!(
            "series list ready kind=3 sections={} eps={} rows={}",
            pgc.season.sections.len(),
            pgc.season.all_episodes().count(),
            state.rows.len()
        ));
        for (index, row) in state.rows.iter().enumerate().take(12) {
            crate::debug_log(&format!(
                "series row i={index} header={} current={} label={:?} title={:?} dur={:?}",
                u8::from(row.header),
                u8::from(row.current),
                row.label.as_str(),
                row.title.as_str(),
                row.duration_text.as_str()
            ));
        }
        publish_series_state(&weak, &series_state, state);
    } else
    {
        let weak = weak.clone();
        let series_state = series_state.clone();
        let session = session.clone();
        let open_bvid = video.bvid.clone();
        let open_cid = cid;
        crate::netpool::submit(
            crate::netpool::Prio::High,
            Box::new(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let detail = match runtime.block_on(session.video_view(&open_bvid)) {
                    Ok(detail) => detail,
                    Err(err) => {
                        crate::debug_log(&format!(
                            "series fetch failed bvid={open_bvid} err={err}"
                        ));
                        return;
                    }
                };
                let state = series_from_detail(&detail, open_cid, &open_bvid);
                let kind = state.kind;
                let sections = detail
                    .season
                    .as_ref()
                    .map(|season| season.sections.len())
                    .unwrap_or(0);
                let episodes = detail
                    .season
                    .as_ref()
                    .map(|season| {
                        season
                            .sections
                            .iter()
                            .map(|section| section.episodes.len())
                            .sum::<usize>()
                    })
                    .unwrap_or(detail.pages.len());
                crate::debug_log(&format!(
                    "series list ready kind={kind} sections={sections} eps={episodes} rows={}",
                    state.rows.len()
                ));
                for (index, row) in state.rows.iter().enumerate().take(12) {
                    crate::debug_log(&format!(
                        "series row i={index} header={} current={} label={:?} title={:?} dur={:?}",
                        u8::from(row.header),
                        u8::from(row.current),
                        row.label.as_str(),
                        row.title.as_str(),
                        row.duration_text.as_str()
                    ));
                }
                // This closure runs on a netpool worker: hand the finished model
                // back to the UI thread before touching the player window. A
                // direct publish here used to fall into `publish_series_state`'s
                // `upgrade()` gate (Slint refuses cross-thread upgrades) and the
                // 选集 button stayed dim for every 分P / 合集 video.
                let publish_weak = weak.clone();
                let publish_state = series_state.clone();
                if slint::invoke_from_event_loop(move || {
                    publish_series_state(&publish_weak, &publish_state, state);
                })
                .is_err()
                {
                    crate::debug_log("series publish skipped reason=event-loop-gone");
                }
            }),
        );
    }
    maybe_start_series_qa(&player, state.main.clone(), series_state.clone());

    // QA hook (`BILI_COMMENT_TOGGLE_TEST=<delay_ms>[,<row>[,<hold_ms>]]`): drive
    // the REAL row-index contract on the open player - open the 评论 tab, toggle
    // a row, log the row's true rpid and the published row, hold the thread open
    // for the capture, then toggle it closed again. The row the UI would send is
    // an index; the id is only ever resolved here, so a rpid past 2^31
    // round-trips intact.
    //
    // `row` omitted = the first row with a 楼中楼 (best chance of being on
    // screen for the shot); `row` = -1 = the first row with MORE than one page
    // (so 查看更多回复 is exercised too). `hold_ms` keeps the thread open long
    // enough for PrintWindow.
    if let Ok(spec) = std::env::var("BILI_COMMENT_TOGGLE_TEST") {
        let mut parts = spec.split(',');
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(9_000)
            .clamp(500, 120_000);
        let wanted_row = parts.next().and_then(|part| part.trim().parse::<i32>().ok());
        let hold = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(12_000)
            .clamp(500, 120_000);
        let session = session.clone();
        let comments = comments.clone();
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let row = match wanted_row {
                Some(row) if row >= 0 => Some(row),
                _ => {
                    let long_thread = wanted_row == Some(-1);
                    let mut chosen = None;
                    for _ in 0..400 {
                        if let Ok(guard) = comments.lock() {
                            if !guard.rows.is_empty() {
                                chosen = if long_thread {
                                    guard
                                        .rows
                                        .iter()
                                        .position(|comment| comment.rcount > 20)
                                        .map(|index| index as i32)
                                } else {
                                    // The busiest thread among the first three
                                    // rows, i.e. one that stays on screen for
                                    // the capture.
                                    guard
                                        .rows
                                        .iter()
                                        .take(3)
                                        .enumerate()
                                        .filter(|(_, comment)| comment.rcount > 0)
                                        .max_by_key(|(_, comment)| comment.rcount)
                                        .map(|(index, _)| index as i32)
                                        .or_else(|| {
                                            guard
                                                .rows
                                                .iter()
                                                .position(|comment| comment.rcount > 0)
                                                .map(|index| index as i32)
                                        })
                                };
                                break;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    chosen
                }
            };
            let Some(row) = row else {
                crate::debug_log("comment-toggle-test no-row-with-replies");
                return;
            };
            // 2 = right panel open on the 评论 tab (`changed right_panel` maps
            // it), so the open thread is on screen for PrintWindow.
            let weak_open = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak_open.upgrade() {
                    player.set_right_panel(2);
                    player.set_right_tab(1);
                    // `changed right_panel` only remaps the tab; the window
                    // grows in Rust's `panel_changed` handler.
                    player.invoke_panel_changed();
                    crate::debug_log(&format!(
                        "comment-toggle-test panel open right_panel={} right_tab={}",
                        player.get_right_panel(),
                        player.get_right_tab()
                    ));
                }
            });
            std::thread::sleep(Duration::from_millis(700));
            let root_rpid = comment_row_rpid(&comments, row);
            if let Some(rpid) = root_rpid {
                crate::debug_log(&format!(
                    "comment-toggle-test row={row} rpid={rpid} fits_i32={}",
                    i32::try_from(rpid).is_ok()
                ));
            }
            on_comment_toggle(session.clone(), comments.clone(), row);
            std::thread::sleep(Duration::from_millis(2_500));
            log_published_comment_row(&weak, row, "open");
            // 查看更多回复 goes through `on_comment_replies(row, pn)` - the same
            // row index, the next page of the SAME parent thread.
            if let Some(root_rpid) = root_rpid {
                let next_page = match comments.lock() {
                    Ok(guard) => {
                        let loaded = guard.replies.get(&root_rpid).map(Vec::len).unwrap_or(0);
                        guard
                            .reply_more
                            .get(&root_rpid)
                            .copied()
                            .unwrap_or(false)
                            .then(|| (loaded / 20 + 1) as i64)
                    }
                    Err(_) => None,
                };
                if let Some(pn) = next_page {
                    crate::debug_log(&format!(
                        "comment-toggle-test more row={row} rpid={root_rpid} pn={pn}"
                    ));
                    on_comment_replies(session.clone(), comments.clone(), row, pn);
                    std::thread::sleep(Duration::from_millis(2_000));
                    log_published_comment_row(&weak, row, "more");
                }
            }
            std::thread::sleep(Duration::from_millis(hold));
            on_comment_toggle(session, comments, row);
            std::thread::sleep(Duration::from_millis(900));
            log_published_comment_row(&weak, row, "closed");
        });
    }

    // The info header's cover: one thumbnail fetch, patched in when it lands.
    if !video.pic.is_empty() {
        let session = session.clone();
        let weak = weak.clone();
        let pic = video.pic.clone();
        crate::netpool::submit(crate::netpool::Prio::Low, Box::new(move || {
            let Some(runtime) = crate::bilibili::shared_runtime() else {
                return;
            };
            let Ok(pixels) = runtime.block_on(session.fetch_thumb(&pic)) else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    player.set_info_cover(to_thumb(&Some(pixels)));
                    player.set_info_cover_ready(true);
                }
            });
        }));
    }

    // The event loop is the presentation thread: keep it above the prefetch
    // workers for the whole process (idempotent, UI thread only).
    player::raise_ui_thread_priority();
    let mini_slot = session_rec.mini_slot.clone();
    // f150: decoding/presentation belong to THIS window, never to focus. The
    // only shared limit is the number of running decoders; the fourth window
    // keeps its state and starts the instant one of the three closes.
    let running = state
        .players
        .borrow()
        .iter()
        .filter(|open| !open.queued.get())
        .count();
    if running > MAX_CONCURRENT_PLAYERS {
        session_rec.queued.set(true);
        *session_rec.deferred.borrow_mut() = Some(DeferredPlayback {
            session: session.clone(),
            video: video.clone(),
            track: track.clone(),
        });
        player.set_playback_queued(true);
        player.set_playing(false);
        player.set_buffering(false);
        player.set_buffer_progress(0.0);
        control.set_paused(true);
        // A queued video is not the queue's now-playing item; it is adopted by
        // `start_queued_playback` when a slot frees.
        set_active_player(&player);
        crate::debug_log(&format!(
            "player queued win={} bvid={} running={running} cap={MAX_CONCURRENT_PLAYERS}",
            session_rec.id, session_rec.video.bvid
        ));
    } else {
        // 播放列表: this window is now the app's active player. Runs before the
        // decode thread starts so the bar shows the title/cover immediately and
        // the queue's own tick can recognize the window.
        if let Some(host) = playlist() {
            host.on_open(&player, &video);
        } else {
            set_active_player(&player);
        }
        player::spawn(session.clone(), video, weak, mini_slot, control.clone(), track);
        crate::debug_log(&format!(
            "player running win={} running={running} cap={MAX_CONCURRENT_PLAYERS}",
            session_rec.id
        ));
        // 进度条悬停预览: 首帧之后在后台拉一次 storyboard（见该函数的注释）。
        // 排队中的窗口不拉: 它还没有解码线程，首帧要等到拿到槽位之后。
        spawn_storyboard_prefetch(player.as_weak(), session.clone(), control.clone());
    }
    // QA (`BILI_F139_TEST`): measure the fast lane against the real decoder and
    // capture `f139_danmaku_<hz>.png` with comments on screen.
    maybe_start_danmaku_frame_qa(&player, danmaku_layer.clone());
    // QA (`BILI_INTERP_FPS_TEST=120`): press the 帧率 row after `_MS` (default
    // 6 s) through the window's own callback, so the 120 fps self-check runs
    // exactly as a click makes it run (restart, 3 s measurement, keep/refuse).
    if let Some(value) = crate::qa::env::var("BILI_INTERP_FPS_TEST") {
        if let Ok(hz) = value.trim().parse::<i32>() {
            let delay = crate::qa::env::var("BILI_INTERP_FPS_MS")
                .and_then(|ms| ms.trim().parse::<u64>().ok())
                .unwrap_or(6_000)
                .clamp(1_000, 60_000);
            let player_weak = player.as_weak();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(player) = player_weak.upgrade() {
                        crate::debug_log(&format!("interp fps test press hz={hz}"));
                        player.invoke_interp_fps(hz >= 120);
                    }
                });
            });
        }
    }
    // The new window takes focus: card_open(0) replaces IT, and a later click on
    // an older window moves focus there (`poll_player_focus`).
    focus_player(state, &session_rec, false);
    // f179 QA (`BILI_F179_TEST=1`): one line with the toolbar grid AS LAID OUT
    // after the first layout pass - the cells, the two gap tokens, the pill's
    // width and the 信息 cell's own rect, so the report proves the new cell sits
    // inside the pill at its neighbours' size instead of trusting the source.
    if crate::qa::env::flag("BILI_F179_TEST") {
        let weak = player.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(2500), move || {
            let Some(p) = weak.upgrade() else {
                return;
            };
            let x = |value: f32| value;
            let info_x = x(p.get_qa_info_btn_x());
            let info_w = x(p.get_qa_info_btn_w());
            let dm_x = x(p.get_qa_dm_btn_x2());
            let dm_w = x(p.get_qa_dm_btn_w2());
            let cm_x = x(p.get_qa_cm_btn_x());
            crate::debug_log(&format!(
                "f179 toolbar cell={:.0} gap={:.0} group_gap={:.0} content_w={:.0} bar_w={:.0} \
                 pill_x={:.1} pill_w={:.1} play={:.1}+{:.1} time_w={:.1} dm={:.1}+{:.1} \
                 info={:.1}+{:.1}@y{:.1} radius={:.0} visible={} cm={:.1}+{:.1} \
                 series={:.1}+{:.1} gap_before_info={:.1} gap_after_info={:.1} \
                 info_right={:.1} pill_right={:.1}",
                x(p.get_qa_bar_cell()),
                x(p.get_qa_bar_gap()),
                x(p.get_qa_bar_group_gap()),
                x(p.get_qa_bar_content_w()),
                x(p.get_qa_bar_w()),
                x(p.get_qa_pill_x()),
                x(p.get_qa_pill_w()),
                x(p.get_qa_play_x()),
                x(p.get_qa_play_w()),
                x(p.get_qa_time_w()),
                dm_x,
                dm_w,
                info_x,
                info_w,
                x(p.get_qa_info_btn_y()),
                x(p.get_qa_info_btn_radius()),
                p.get_qa_info_btn_visible(),
                cm_x,
                x(p.get_qa_cm_btn_w()),
                x(p.get_qa_series_btn_x()),
                x(p.get_qa_series_btn_w()),
                info_x - (dm_x + dm_w),
                cm_x - (info_x + info_w),
                info_x + info_w,
                x(p.get_qa_pill_x()) + x(p.get_qa_pill_w()),
            ));
        });
    }
    maybe_start_window_probe(state);
}

// ===================== 进度条悬停预览 (storyboard) =====================
//
// B站 web 播放器 hover 进度条时显示的那张帧预览，来自 `x/player/videoshot`
// 返回的雪碧图 (storyboard)：`cols x rows` 格（实测 10x10），每格 480x270，
// 一组 1..N 张 jpg，`index` 给出每格的时间戳（秒）。这里把它做成一张**单一
// atlas**：每格缩到 160x90（预览卡的实际尺寸），第 s 张图的第 r 行落在
// atlas 的第 `s * rows + r` 行，所以第 k 格永远是
//     x = (k % cols) * 160,  y = (k / cols) * 90
// 与“第几张图”无关——格子是连续编号的，这个式子跨图也成立。
//
// 时间 -> 格子：`times[k]` 是第 k 格对应的时间，取**最后一个 `times[k] <= t`**
// （一个格子代表从它自己的时间开始的那一段）。三支实测视频核对过：
//   29 s   times 0,5,10,15,20,25                    (5 s 步长)
//   356 s  times 0,5,…,355                          (5 s 步长)
//   3203 s times 0,10,17,24,…,3195                  (开头 10 s，之后 7 s)
// UI 不做二分：Rust 把 1 s 一档的“时间桶 -> 格子”表 (`sb_cells`) 一次发布，
// Slint 只做 `min(floor(ms / 1000), len - 1)` 的下标（O(1)，没有回调、没有每帧
// 分配），所以指针移动时 UI 线程只改 source-clip 的四个数。
//
// 拉取时机：首帧到达且正在播放之后，在 netpool (Low) 里拉一次；换集时
// `source_seq` 变化再拉一次。暂停/遮挡不额外拉取，hover 只读已经解码好的图。

/// 预览格子在图集里的尺寸。必须与 `ui/app.slint` 的预览卡图窗一致。
pub(crate) const STORYBOARD_CELL_W: u32 = 160;
pub(crate) const STORYBOARD_CELL_H: u32 = 90;
/// 时间桶宽度 (ms)：一格代表 1 s，远小于最短的 5 s 格距。
pub(crate) const STORYBOARD_BUCKET_MS: u64 = 1000;
/// 图集上限（格数）：160x90 RGBA 每格 57.6 KB，900 格约 51 MB，够 1.7 h
/// (7 s/格) 的视频；更长的视频保留前段，尾部沿用最后一格。
pub(crate) const STORYBOARD_MAX_TILES: usize = 900;
/// 最多下载的雪碧图张数（10x10 一张 100 格）。
pub(crate) const STORYBOARD_MAX_SHEETS: usize = 9;

/// 1 s 一档的“毫秒 -> 格子”表: `cells[i]` 是第 i 秒开始的格子。
///
/// 即“第 t 秒对应最后一个 `times[k] <= t` 的格子”，在 Rust 里一次算完；
/// UI 只做一次数组下标，指针移动时没有回调、没有分配。
pub(crate) fn storyboard_buckets(times: &[f32], duration_ms: u64) -> Vec<i32> {
    if times.is_empty() {
        return Vec::new();
    }
    let seconds = duration_ms / STORYBOARD_BUCKET_MS + 1;
    let end = (times.last().copied().unwrap_or(0.0).ceil() as u64) + 1;
    let buckets = seconds.max(end) as usize;
    let mut out = Vec::with_capacity(buckets);
    let mut k = 0usize;
    for bucket in 0..buckets {
        let at = (bucket as f32) * (STORYBOARD_BUCKET_MS as f32) / 1000.0;
        while k + 1 < times.len() && times[k + 1] <= at {
            k += 1;
        }
        out.push(k as i32);
    }
    out
}

/// 把一张雪碧图整幅（`sw x sh`）盒式缩到 `aw x dh_full`，写进 atlas 的
/// `oy` 行起。超出 atlas 底部的行丢弃（最后一张图通常只用得到一部分）。
fn blit_sheet(
    src: &[u8],
    sw: usize,
    sh: usize,
    dst: &mut [u8],
    aw: usize,
    ah: usize,
    oy: usize,
    dh_full: usize,
) {
    if sw == 0 || sh == 0 || aw == 0 || dh_full == 0 {
        return;
    }
    let rows = dh_full.min(ah.saturating_sub(oy));
    for dy in 0..rows {
        let y0 = dy * sh / dh_full;
        let y1 = (((dy + 1) * sh / dh_full).max(y0 + 1)).min(sh);
        for dx in 0..aw {
            let x0 = dx * sw / aw;
            let x1 = (((dx + 1) * sw / aw).max(x0 + 1)).min(sw);
            let mut sum = [0u32; 3];
            let mut count = 0u32;
            for y in y0..y1 {
                let row = y * sw;
                for x in x0..x1 {
                    let at = (row + x) * 4;
                    if at + 3 >= src.len() {
                        continue;
                    }
                    sum[0] += u32::from(src[at]);
                    sum[1] += u32::from(src[at + 1]);
                    sum[2] += u32::from(src[at + 2]);
                    count += 1;
                }
            }
            let at = ((oy + dy) * aw + dx) * 4;
            if at + 3 >= dst.len() || count == 0 {
                continue;
            }
            dst[at] = (sum[0] / count) as u8;
            dst[at + 1] = (sum[1] / count) as u8;
            dst[at + 2] = (sum[2] / count) as u8;
            dst[at + 3] = 255;
        }
    }
}

/// 首帧之后在后台拉一次本视频的 storyboard；换集时按 `source_seq` 重拉。
pub(crate) fn spawn_storyboard_prefetch(
    weak: Weak<PlayerWindow>,
    session: Arc<bilibili::Session>,
    control: Arc<player::PlaybackControl>,
) {
    std::thread::spawn(move || {
        // 等首帧：预取只在画面真的出来之后开始（暂停时也不拉）。
        let started = Instant::now();
        loop {
            if control.is_closed() {
                return;
            }
            if control.presented_frames() > 0 && !control.paused() {
                break;
            }
            if started.elapsed() > Duration::from_secs(45) {
                crate::debug_log("storyboard skip reason=no-first-frame");
                return;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        let mut seq = control.source_seq();
        loop {
            let source = control.source();
            if source.cid != 0 && !source.bvid.trim().is_empty() {
                fetch_storyboard(
                    weak.clone(),
                    session.clone(),
                    control.clone(),
                    seq,
                    source.bvid.clone(),
                    source.cid,
                );
            }
            // 换集 / 换视频：等 source_seq 变化，清掉旧图再拉新的。
            loop {
                std::thread::sleep(Duration::from_millis(400));
                if control.is_closed() {
                    return;
                }
                let next = control.source_seq();
                if next != seq {
                    seq = next;
                    break;
                }
            }
            let clear_target = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = clear_target.upgrade() {
                    clear_storyboard(&window);
                }
            });
        }
    });
}

/// Clear the preview properties (a switch: the old video's atlas must never be
/// shown against the new timeline).
fn clear_storyboard(window: &PlayerWindow) {
    window.set_sb_ready(false);
    window.set_sb_image(Image::default());
    window.set_sb_cells(ModelRc::from(Rc::new(VecModel::<i32>::from(Vec::<i32>::new()))));
}

/// Download + decode + stitch one video's storyboard, on the netpool.
fn fetch_storyboard(
    weak: Weak<PlayerWindow>,
    session: Arc<bilibili::Session>,
    control: Arc<player::PlaybackControl>,
    seq: u64,
    bvid: String,
    cid: u64,
) {
    crate::netpool::submit(
        crate::netpool::Prio::Low,
        Box::new(move || {
            let Some(runtime) = crate::bilibili::shared_runtime() else {
                return;
            };
            let started = Instant::now();
            let shot = match runtime.block_on(session.videoshot(&bvid, cid)) {
                Ok(shot) => shot,
                Err(err) => {
                    // No storyboard for this video (or no login): the UI simply
                    // keeps showing the time alone. Never a user-facing error.
                    crate::debug_log(&format!(
                        "storyboard none bvid={bvid} cid={cid} err={err:#}"
                    ));
                    return;
                }
            };
            let cols = shot.cols as usize;
            let rows = shot.rows as usize;
            let per_sheet = cols * rows;
            let sheets_needed = shot
                .sheets
                .len()
                .min(STORYBOARD_MAX_SHEETS)
                .min(per_sheet.max(1));
            let tiles = shot
                .times
                .len()
                .min(per_sheet * sheets_needed)
                .min(STORYBOARD_MAX_TILES);
            if cols == 0 || rows == 0 || tiles == 0 || sheets_needed == 0 {
                crate::debug_log(&format!("storyboard empty bvid={bvid} cid={cid}"));
                return;
            }
            let aw = cols * STORYBOARD_CELL_W as usize;
            let ah_rows = (tiles + cols - 1) / cols;
            let ah = ah_rows * STORYBOARD_CELL_H as usize;
            let mut atlas = vec![0u8; aw * ah * 4];
            for (index, url) in shot.sheets.iter().take(sheets_needed).enumerate() {
                if control.is_closed() || control.source_seq() != seq {
                    return;
                }
                let decoded = match runtime.block_on(session.fetch_image(url)) {
                    Ok(decoded) => decoded,
                    Err(err) => {
                        crate::debug_log(&format!(
                            "storyboard sheet {index} err={err:#} bvid={bvid}"
                        ));
                        return;
                    }
                };
                let (bytes, width, height) = decoded;
                let oy = index * rows * STORYBOARD_CELL_H as usize;
                if oy >= ah {
                    break;
                }
                blit_sheet(
                    &bytes,
                    width as usize,
                    height as usize,
                    &mut atlas,
                    aw,
                    ah,
                    oy,
                    rows * STORYBOARD_CELL_H as usize,
                );
            }
            let duration = control.duration_ms();
            let times = &shot.times[..tiles];
            let cells = storyboard_buckets(times, duration);
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            let target = weak.clone();
            let control_after = control.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if control_after.is_closed() || control_after.source_seq() != seq {
                    return;
                }
                let Some(window) = target.upgrade() else {
                    return;
                };
                let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(aw as u32, ah as u32);
                buffer.make_mut_bytes().copy_from_slice(&atlas);
                window.set_sb_cells(ModelRc::from(Rc::new(VecModel::from(cells))));
                window.set_sb_cols(cols as i32);
                window.set_sb_image(Image::from_rgba8(buffer));
                window.set_sb_ready(true);
                crate::debug_log(&format!(
                    "storyboard ready bvid={bvid} cid={cid} sheets={sheets_needed} tiles={tiles} \
                     atlas={aw}x{ah} fetch_ms={elapsed:.0}"
                ));
            });
        }),
    );
}


/// Load the first page of `aid`'s comments, replacing any current state.
///
/// Entry point for `on_comments_load`.
pub(crate) fn on_comments_load(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    aid: i64,
    sort_hot: bool,
) {
    if aid == 0 {
        return;
    }
    if let Ok(mut guard) = comments.lock() {
        // The window handle survives a reload (a sort change or 评论 refresh
        // must keep publishing to the same player).
        let ui = guard.ui.clone();
        *guard = CommentsState {
            aid,
            sort_hot,
            ui,
            ..CommentsState::default()
        };
    }
    comments_fetch(session, comments, aid, sort_hot, String::new(), false);
}

/// Continue the current list from its cursor. Entry point for
/// `on_comments_more`; a no-op while a page is in flight or the list has ended.
///
/// The page fetched ahead of the UI is consumed first: appending it needs no
/// network and shows no loading skeleton, so a scroll is instant. Only when no
/// buffer matches the current cursor does this fall back to a real fetch.
pub(crate) fn on_comments_more(session: Arc<bilibili::Session>, comments: CommentsSlot) {
    let buffered = match comments.lock() {
        Ok(mut guard) => {
            let cursor = guard.cursor.clone();
            let aid = guard.aid;
            let sort_hot = guard.sort_hot;
            match guard.prefetch.take() {
                Some(buffer)
                    if buffer.aid == aid
                        && buffer.sort_hot == sort_hot
                        && buffer.cursor == cursor =>
                {
                    Some(buffer)
                }
                other => {
                    // Stale buffer (another cursor / video / sort): keep it
                    // only if it still could be used, otherwise drop it.
                    guard.prefetch = other;
                    None
                }
            }
        }
        Err(_) => return,
    };
    if let Some(buffer) = buffered {
        let rows = buffer.page.comments.len();
        let absorbed = match comments.lock() {
            Ok(mut guard) => {
                comments_absorb(&mut guard, buffer.aid, buffer.sort_hot, buffer.page, true)
            }
            Err(_) => None,
        };
        if let Some((cursor, is_end)) = absorbed {
            crate::debug_log(&format!(
                "comments more from prefetch n={rows} next={} is_end={is_end}",
                !cursor.is_empty()
            ));
            publish_comments(&comments);
            comments_prefetch_next(session, comments);
            return;
        }
    }
    let (aid, sort_hot, cursor, is_end, loading) = match comments.lock() {
        Ok(guard) => (
            guard.aid,
            guard.sort_hot,
            guard.cursor.clone(),
            guard.is_end,
            guard.loading,
        ),
        Err(_) => return,
    };
    if aid == 0 || is_end || loading || cursor.is_empty() {
        return;
    }
    comments_fetch(session, comments, aid, sort_hot, cursor, true);
}

/// Switch between 热门 (`sort_hot = true`) and 按时间 (`false`) and reload the
/// first page. Entry point for `on_comments_sort`.
pub(crate) fn on_comments_sort(session: Arc<bilibili::Session>, comments: CommentsSlot, sort_hot: bool) {
    let aid = match comments.lock() {
        Ok(guard) => guard.aid,
        Err(_) => return,
    };
    if aid == 0 {
        return;
    }
    on_comments_load(session, comments, aid, sort_hot);
}

/// Root `rpid` for a row index the UI sent back, from the map published with
/// the current model. `None` covers a negative / out-of-range row (a click that
/// was queued while the list was replaced by a sort change or another video).
pub(crate) fn comment_row_rpid(comments: &CommentsSlot, row: i32) -> Option<i64> {
    let row = usize::try_from(row).ok()?;
    let guard = comments.lock().ok()?;
    guard.row_rpids.get(row).copied().filter(|rpid| *rpid > 0)
}

/// Fetch one comment's dialog. Entry point for `on_comment_replies`; `row` is
/// the 楼中楼's parent row in `comment_items` and `pn` is 1-based. The row is
/// resolved to its root `rpid` here, then the reply rows are cached under that
/// rpid - so a reply keeps its own rpid for later 查看更多回复 pages.
pub(crate) fn on_comment_replies(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    row: i32,
    pn: i64,
) {
    let Some(root_rpid) = comment_row_rpid(&comments, row) else {
        crate::debug_log(&format!(
            "comment replies row={row} pn={pn} ignored=out-of-range"
        ));
        return;
    };
    crate::debug_log(&format!(
        "comment replies row={row} rpid={root_rpid} pn={pn}"
    ));
    comment_replies_fetch(session, comments, root_rpid, pn);
}

/// Toggle one root comment's 楼中楼. Entry point for the panel's row click;
/// `row` indexes `comment_items` and is resolved to the root `rpid` before the
/// thread state is touched.
///
/// `expanded` lives in [`CommentsState`], not in the model row, so a republish
/// (load more, sort change, avatar patch) keeps every thread the user opened
/// open. Opening a thread fetches page 1 when its dialog is not cached yet.
pub(crate) fn on_comment_toggle(session: Arc<bilibili::Session>, comments: CommentsSlot, row: i32) {
    let Some(root_rpid) = comment_row_rpid(&comments, row) else {
        crate::debug_log(&format!("comment toggle row={row} ignored=out-of-range"));
        return;
    };
    let (fetch, expanded) = match comments.lock() {
        Ok(mut guard) => {
            if guard.expanded.remove(&root_rpid) {
                (false, false)
            } else {
                guard.expanded.insert(root_rpid);
                (!guard.replies.contains_key(&root_rpid), true)
            }
        }
        Err(_) => return,
    };
    crate::debug_log(&format!(
        "comment toggle row={row} rpid={root_rpid} expanded={expanded} fetch={fetch}"
    ));
    publish_comments(&comments);
    if fetch {
        comment_replies_fetch(session, comments, root_rpid, 1);
    }
}

/// Push the current comment state to the open player's 评论区.
///
/// The model build runs inside `invoke_from_event_loop` (`Image` is not `Send`),
/// so this is safe from any thread; a closed window or the headless
/// `BILI_COMMENTS_TEST` path (no `ui` handle) is a no-op.
pub(crate) fn publish_comments(comments: &CommentsSlot) {
    let ui = match comments.lock() {
        Ok(guard) => guard.ui.clone(),
        Err(_) => None,
    };
    let Some(ui) = ui else {
        return;
    };
    let comments = comments.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = ui.upgrade() else {
            return;
        };
        let (items, row_rpids, loading, error, total, is_end, sort_hot) = {
            let Ok(guard) = comments.lock() else {
                return;
            };
            let items: Vec<CommentRow> = guard
                .rows
                .iter()
                .map(|row| build_comment_row(row, &guard))
                .collect();
            // Same snapshot as `items`, so index i in the model is rpid
            // `row_rpids[i]` for every callback that arrives about this list.
            let row_rpids: Vec<i64> = guard.rows.iter().map(|row| row.rpid).collect();
            (
                items,
                row_rpids,
                guard.loading,
                guard.error.clone(),
                guard.total,
                guard.is_end,
                guard.sort_hot,
            )
        };
        // Publish the mapping just before the model that uses it; both writes
        // are on the UI thread, so a click can never see one without the other.
        if let Ok(mut guard) = comments.lock() {
            guard.row_rpids = row_rpids.clone();
        }
        // What the row-chip state ended up as after this republish (the QA
        // proof that ``expanded`` reached the model, including for a row whose
        // rpid does not fit in 32 bits).
        for (index, item) in items.iter().enumerate() {
            if item.expanded {
                crate::debug_log(&format!(
                    "comment publish row={index} rpid={} replies={} total={} expanded=1",
                    row_rpids.get(index).copied().unwrap_or(0),
                    item.replies.row_count(),
                    item.replies_text,
                ));
            }
        }
        sync_comment_model(&window, items);
        window.set_comments_loading(loading);
        window.set_comments_error(error.into());
        window.set_comments_total(to_i32(total));
        window.set_comments_end(is_end);
        window.set_comments_hot(sort_hot);
    });
}

/// Update the panel's model in place when the list only grew or changed state
/// (load more, expand, avatar), so the ListView keeps its scroll position; a
/// head that no longer matches (sort change / other video) replaces the model.
pub(crate) fn sync_comment_model(window: &PlayerWindow, items: Vec<CommentRow>) {
    let model = window.get_comment_items();
    let Some(vec) = model.as_any().downcast_ref::<VecModel<CommentRow>>() else {
        window.set_comment_items(ModelRc::from(Rc::new(VecModel::from(items))));
        return;
    };
    let same_head = vec.row_count() > 0
        && items.first().map(|row| row.rpid) == vec.row_data(0).map(|row| row.rpid);
    if same_head && items.len() >= vec.row_count() {
        for (index, item) in items.into_iter().enumerate() {
            if index < vec.row_count() {
                vec.set_row_data(index, item);
            } else {
                vec.push(item);
            }
        }
        return;
    }
    vec.set_vec(items);
}

/// One panel row: the comment plus the thread state (avatar from the pixel
/// cache, expanded set, fetched dialog) the UI needs.
pub(crate) fn build_comment_row(row: &bilibili::Comment, state: &CommentsState) -> CommentRow {
    let cached = state.replies.get(&row.rpid);
    let expanded = state.expanded.contains(&row.rpid);
    let replies: Vec<CommentReply> = match (expanded, cached) {
        (true, Some(rows)) => rows
            .iter()
            .map(|reply| build_reply_row(reply, state))
            .collect(),
        _ => Vec::new(),
    };
    let loaded = cached.map(Vec::len).unwrap_or(0);
    CommentRow {
        rpid: row.rpid as i32,
        uname: row.user.uname.clone().into(),
        message: row.message.clone().into(),
        like_text: like_label(row.like),
        time_text: time_ago(row.ctime).into(),
        replies_text: if row.rcount > 0 {
            format!("{} 条回复", row.rcount)
        } else {
            String::new()
        }
        .into(),
        initial: initial_of(&row.user.uname).into(),
        has_avatar: !row.user.avatar.is_empty() && state.avatars.contains_key(&row.user.avatar),
        avatar: avatar_image(state.avatars.get(&row.user.avatar)),
        expanded,
        next_pn: (loaded / 20 + 1) as i32,
        has_more: expanded && state.reply_more.get(&row.rpid).copied().unwrap_or(false),
        replies: ModelRc::from(Rc::new(VecModel::from(replies))),
    }
}

/// A 楼中楼 row: the flat reply shape (`CommentReply`), which carries no
/// nesting and no 查看更多回复 link of its own.
pub(crate) fn build_reply_row(row: &bilibili::Comment, state: &CommentsState) -> CommentReply {
    CommentReply {
        rpid: row.rpid as i32,
        uname: row.user.uname.clone().into(),
        message: row.message.clone().into(),
        like_text: if row.like > 0 {
            format!("{} 赞", format_view(row.like))
        } else {
            String::new()
        }
        .into(),
        time_text: time_ago(row.ctime).into(),
        initial: initial_of(&row.user.uname).into(),
        has_avatar: !row.user.avatar.is_empty() && state.avatars.contains_key(&row.user.avatar),
        avatar: avatar_image(state.avatars.get(&row.user.avatar)),
    }
}

/// A like count for the meta line (`""` when nobody liked it yet).
pub(crate) fn like_label(count: u64) -> SharedString {
    if count == 0 {
        "赞".into()
    } else {
        format!("{} 赞", format_view(count)).into()
    }
}

/// Placeholder glyph for an avatar that has not arrived: the first character
/// of the user name, which is what the panel shows instead of an empty disc.
pub(crate) fn initial_of(name: &str) -> String {
    name.chars()
        .next()
        .map(|ch| ch.to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Raw RGBA -> `Image` for the avatar slot (`Image::default()` when missing).
pub(crate) fn avatar_image(pixels: Option<&(Vec<u8>, u32, u32)>) -> Image {
    match pixels {
        Some((bytes, width, height)) => {
            let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(*width, *height);
            buffer.make_mut_bytes().copy_from_slice(bytes);
            Image::from_rgba8(buffer)
        }
        None => Image::default(),
    }
}

/// Root rows whose avatar is neither cached nor in flight yet.
pub(crate) fn pending_comment_avatars(comments: &CommentsSlot) -> Vec<(i64, String)> {
    let Ok(guard) = comments.lock() else {
        return Vec::new();
    };
    let mut out: Vec<(i64, String)> = Vec::new();
    for row in &guard.rows {
        let url = &row.user.avatar;
        if url.is_empty() || guard.avatars.contains_key(url) {
            continue;
        }
        if !out.iter().any(|(_, pending)| pending == url) {
            out.push((row.rpid, url.clone()));
        }
    }
    out
}

/// Fetch avatars four at a time and patch each row as it lands, so the list
/// fills progressively instead of waiting for the slowest picture.
pub(crate) fn spawn_comment_avatars(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    pending: Vec<(i64, String)>,
) {
    if pending.is_empty() {
        return;
    }
    crate::netpool::submit(crate::netpool::Prio::Low, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        let mut stream = futures::stream::iter(pending.into_iter().map(|(rpid, url)| {
            let session = session.clone();
            async move {
                let pixels = session.fetch_avatar(&url).await.ok();
                (rpid, url, pixels)
            }
        }))
        .buffer_unordered(4);
        while let Some((rpid, url, pixels)) = runtime.block_on(stream.next()) {
            let Some(pixels) = pixels else {
                continue;
            };
            if let Ok(mut guard) = comments.lock() {
                guard.avatars.insert(url, pixels.clone());
            }
            let ui = match comments.lock() {
                Ok(guard) => guard.ui.clone(),
                Err(_) => None,
            };
            let Some(ui) = ui else {
                continue;
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = ui.upgrade() {
                    patch_comment_avatar(&window, rpid, &pixels);
                }
            });
        }
    }));
}

/// Replace one avatar in the rendered model (root rows and their dialogs),
/// leaving every text/state field untouched.
pub(crate) fn patch_comment_avatar(window: &PlayerWindow, rpid: i64, pixels: &(Vec<u8>, u32, u32)) {
    let image = avatar_image(Some(pixels));
    let model = window.get_comment_items();
    let Some(vec) = model.as_any().downcast_ref::<VecModel<CommentRow>>() else {
        return;
    };
    for index in 0..vec.row_count() {
        let Some(mut row) = vec.row_data(index) else {
            continue;
        };
        let mut changed = false;
        if row.rpid == rpid as i32 && !row.has_avatar {
            row.avatar = image.clone();
            row.has_avatar = true;
            changed = true;
        }
        if let Some(replies) = row
            .replies
            .as_any()
            .downcast_ref::<VecModel<CommentReply>>()
        {
            for reply_index in 0..replies.row_count() {
                let Some(mut reply) = replies.row_data(reply_index) else {
                    continue;
                };
                if reply.rpid == rpid as i32 && !reply.has_avatar {
                    reply.avatar = image.clone();
                    reply.has_avatar = true;
                    replies.set_row_data(reply_index, reply);
                }
            }
        }
        if changed {
            vec.set_row_data(index, row);
        }
    }
}

/// Fold one landed page into the state.
///
/// Returns the new `(cursor, is_end)` when the page belonged to the current
/// list, and `None` when a newer load (sort change / other video) owns the slot
/// and the caller must drop it.
pub(crate) fn comments_absorb(
    state: &mut CommentsState,
    aid: i64,
    sort_hot: bool,
    page: bilibili::CommentPage,
    append: bool,
) -> Option<(String, bool)> {
    if state.aid != aid || state.sort_hot != sort_hot {
        return None;
    }
    let bilibili::CommentPage {
        top,
        comments,
        cursor,
        is_end,
        total,
    } = page;
    state.cursor = cursor;
    state.is_end = is_end;
    state.total = total;
    if append {
        state.rows.extend(comments);
    } else {
        state.top = top;
        state.rows = comments;
        state.replies.clear();
        state.reply_more.clear();
        state.expanded.clear();
    }
    state.loading = false;
    Some((state.cursor.clone(), state.is_end))
}

/// Fetch the page after the current cursor into the slot, so the next
/// `comments_more()` appends it without a round trip.
///
/// Never fetches past `is_end`, never duplicates an in-flight request, and
/// keeps at most one buffered page. A page that lands after the cursor moved
/// (another video / sort / a real `more`) is dropped.
pub(crate) fn comments_prefetch_next(session: Arc<bilibili::Session>, comments: CommentsSlot) {
    let (aid, sort_hot, cursor) = match comments.lock() {
        Ok(mut guard) => {
            if guard.aid == 0
                || guard.is_end
                || guard.cursor.is_empty()
                || guard.prefetch.is_some()
                || guard.prefetching
            {
                return;
            }
            guard.prefetching = true;
            (guard.aid, guard.sort_hot, guard.cursor.clone())
        }
        Err(_) => return,
    };
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            if let Ok(mut guard) = comments.lock() {
                guard.prefetching = false;
            }
            return;
        };
        let result = runtime.block_on(session.comments(aid, sort_hot, &cursor));
        match result {
            Ok(page) => {
                crate::debug_log(&format!(
                    "comments prefetch aid={aid} sort={} n={} next={} is_end={} total={}",
                    if sort_hot { "hot" } else { "time" },
                    page.comments.len(),
                    !page.cursor.is_empty(),
                    page.is_end,
                    page.total
                ));
                if let Ok(mut guard) = comments.lock() {
                    guard.prefetching = false;
                    // Only buffer a page that still belongs to this cursor.
                    if guard.aid == aid
                        && guard.sort_hot == sort_hot
                        && guard.cursor == cursor
                        && !guard.is_end
                    {
                        guard.prefetch = Some(CommentsPrefetch {
                            aid,
                            sort_hot,
                            cursor,
                            page,
                        });
                    }
                }
            }
            Err(err) => {
                crate::debug_log(&format!(
                    "comments prefetch error aid={aid} sort={} err={err}",
                    if sort_hot { "hot" } else { "time" }
                ));
                if let Ok(mut guard) = comments.lock() {
                    guard.prefetching = false;
                }
            }
        }
    }));
}

/// Fetch one page of comments and store it in `comments`.
///
/// `append` continues the list (评论加载更多); otherwise the page replaces it
/// (first load / sort change). Runs on its own thread over the shared runtime,
/// like every other list worker, and fetches the page after it into the buffer
/// as soon as this one lands.
pub(crate) fn comments_fetch(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    aid: i64,
    sort_hot: bool,
    cursor: String,
    append: bool,
) {
    if let Ok(mut guard) = comments.lock() {
        guard.loading = true;
        guard.error.clear();
    }
    // Show the loading bar immediately (the worker may take a second).
    publish_comments(&comments);
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            if let Ok(mut guard) = comments.lock() {
                guard.loading = false;
            }
            publish_comments(&comments);
            return;
        };
        match runtime.block_on(session.comments(aid, sort_hot, &cursor)) {
            Ok(page) => {
                crate::debug_log(&format!(
                    "comments loaded aid={aid} sort={} append={append} n={} next={} is_end={} total={}",
                    if sort_hot { "hot" } else { "time" },
                    page.comments.len(),
                    !page.cursor.is_empty(),
                    page.is_end,
                    page.total
                ));
                // A newer load (sort change / other video) wins.
                let absorbed = match comments.lock() {
                    Ok(mut guard) => comments_absorb(&mut guard, aid, sort_hot, page, append),
                    Err(_) => None,
                };
                if absorbed.is_none() {
                    return;
                }
                // Publish the rows, then stream their avatars in.
                publish_comments(&comments);
                let pending = pending_comment_avatars(&comments);
                spawn_comment_avatars(session.clone(), comments.clone(), pending);
                // One page ahead, so the next scroll never waits on the network.
                comments_prefetch_next(session.clone(), comments.clone());
            }
            Err(err) => {
                if let Ok(mut guard) = comments.lock() {
                    guard.loading = false;
                    guard.error = format!("{err}");
                    crate::debug_log(&format!("comments error aid={aid} err={}", guard.error));
                }
                // The panel shows this real error text under the list.
                publish_comments(&comments);
            }
        }
    }));
}

/// Fetch one comment's dialog and cache its rows under `root_rpid`.
pub(crate) fn comment_replies_fetch(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    root_rpid: i64,
    pn: i64,
) {
    let aid = match comments.lock() {
        Ok(guard) => guard.aid,
        Err(_) => return,
    };
    if aid == 0 || root_rpid == 0 {
        return;
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        match runtime.block_on(session.comment_replies(aid, root_rpid, pn)) {
            Ok(page) => {
                crate::debug_log(&format!(
                    "comment replies loaded aid={aid} root={root_rpid} pn={pn} n={} total={} page={} size={} has_more={}",
                    page.replies.len(),
                    page.total,
                    page.page,
                    page.size,
                    page.has_more
                ));
                // `has_more` must be read before the rows move into the cache.
                let has_more = page.has_more && !page.replies.is_empty();
                if let Ok(mut guard) = comments.lock() {
                    if guard.aid != aid {
                        return;
                    }
                    let entry = guard.replies.entry(root_rpid).or_default();
                    if pn <= 1 {
                        *entry = page.replies;
                    } else {
                        // 查看更多回复: append the next page, skipping any row
                        // the cache already holds.
                        for row in page.replies {
                            if !entry.iter().any(|existing| existing.rpid == row.rpid) {
                                entry.push(row);
                            }
                        }
                    }
                    guard.reply_more.insert(root_rpid, has_more);
                }
                // Publish the thread, then stream its avatars in.
                publish_comments(&comments);
                let pending: Vec<(i64, String)> = match comments.lock() {
                    Ok(guard) => guard
                        .replies
                        .get(&root_rpid)
                        .map(|rows| {
                            rows.iter()
                                .filter(|row| {
                                    !row.user.avatar.is_empty()
                                        && !guard.avatars.contains_key(&row.user.avatar)
                                })
                                .map(|row| (row.rpid, row.user.avatar.clone()))
                                .collect()
                        })
                        .unwrap_or_default(),
                    Err(_) => Vec::new(),
                };
                spawn_comment_avatars(session.clone(), comments.clone(), pending);
            }
            Err(err) => {
                crate::debug_log(&format!(
                    "comment replies error aid={aid} root={root_rpid} pn={pn} err={err}"
                ));
            }
        }
    }));
}
