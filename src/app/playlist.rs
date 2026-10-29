//! The playback queue and playlist host.

use crate::*;

/// One queued video; fields mirror `settings::QueueEntry` plus what the player
/// needs to open it.
#[derive(Clone, Debug)]
pub(crate) struct QueueEntry {
    pub(crate) bvid: String,
    pub(crate) cid: u64,
    pub(crate) aid: i64,
    pub(crate) title: String,
    pub(crate) owner: String,
    pub(crate) duration: String,
    /// Cover URL exactly as the card handed it out (empty = no cover).
    pub(crate) cover: String,
}

impl QueueEntry {
    pub(crate) fn from_feed(video: &bilibili::FeedVideo) -> Self {
        Self {
            bvid: video.bvid.clone(),
            cid: video.cid,
            aid: video.aid as i64,
            title: video.title.clone(),
            owner: video.owner.clone(),
            duration: format_duration(video.duration),
            cover: video.pic.clone(),
        }
    }

    pub(crate) fn from_settings(entry: &settings::QueueEntry) -> Self {
        Self {
            bvid: entry.bvid.clone(),
            cid: entry.cid,
            aid: entry.aid,
            title: entry.title.clone(),
            owner: entry.owner.clone(),
            duration: entry.duration.clone(),
            cover: entry.cover.clone(),
        }
    }

    pub(crate) fn to_settings(&self) -> settings::QueueEntry {
        settings::QueueEntry {
            bvid: self.bvid.clone(),
            cid: self.cid,
            aid: self.aid,
            title: self.title.clone(),
            owner: self.owner.clone(),
            duration: self.duration.clone(),
            cover: self.cover.clone(),
        }
    }

    /// The video as the existing open-player path expects it. The duration
    /// label is parsed back to seconds so the info header's stats still read
    /// correctly; anything unparseable becomes 0 (the header then omits it).
    pub(crate) fn to_video(&self) -> bilibili::FeedVideo {
        bilibili::FeedVideo {
            bvid: self.bvid.clone(),
            cid: self.cid,
            aid: self.aid.max(0) as u64,
            title: self.title.clone(),
            pic: self.cover.clone(),
            owner: self.owner.clone(),
            owner_mid: 0,
            view: 0,
            duration: duration_seconds(&self.duration),
            pub_ts: 0,
        }
    }
}

/// Ring state for one queued item: `(0..1 progress, complete)`.
///
/// The preload registry is keyed by the cache key `<bvid>-<cid>-<quality>-
/// <codecid>`, while a queue row only knows the first two fields. Matching the
/// prefix keeps the ring independent of which quality/codec the preload chose,
/// and an audio file (`<bvid>-<cid>-audio-…`) can never make a video ring full
/// because only video tracks are preloaded by the playlist path.
pub(crate) fn preload_state(
    snapshot: &[(String, u64, u64, bool)],
    bvid: &str,
    cid: u64,
) -> (f32, bool) {
    let prefix = format!("{bvid}-{cid}-");
    let mut best = (0.0f32, false);
    for (key, done, total, complete) in snapshot {
        if !key.starts_with(&prefix) {
            continue;
        }
        let fraction = if *complete {
            1.0
        } else if *total > 0 {
            (*done as f64 / *total as f64).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        if fraction > best.0 || (*complete && !best.1) {
            best = (fraction, *complete);
        }
    }
    best
}

/// The app-wide queue host: model, persistence and all the side effects the
/// pure [`PlaybackQueue`] must not know about (windows, covers, logs).
pub(crate) struct Playlist {
    pub(crate) queue: RefCell<PlaybackQueue>,
    pub(crate) ui: Weak<AppWindow>,
    pub(crate) state: std::rc::Weak<PlayerState>,
    pub(crate) session: SessionSlot,
    pub(crate) settings: Arc<Mutex<settings::Settings>>,
    pub(crate) feed: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    pub(crate) browse_bvids: Arc<Mutex<Vec<String>>>,
    pub(crate) space_bvids: Arc<Mutex<Vec<String>>>,
    /// The active player reports "playing" (open, not paused, not buffering).
    pub(crate) playing: Cell<bool>,
    /// A track ended in mode 0: nothing plays until the user starts something.
    pub(crate) stopped: Cell<bool>,
    /// The end of the current item was already handled, so one track can never
    /// advance the queue twice.
    pub(crate) end_handled: Cell<bool>,
    pub(crate) last_tick: Cell<Instant>,
    /// xorshift state for shuffle mode (no new dependency).
    pub(crate) rng: Cell<u64>,
    /// `(current, len)` of the last preload pass: the next-items fetch only
    /// runs when the queue actually changed.
    pub(crate) preload_mark: RefCell<Option<(Option<usize>, usize)>>,
}

pub(crate) fn playlist() -> Option<Rc<Playlist>> {
    PLAYLIST.with(|cell| cell.try_borrow().ok().and_then(|slot| slot.clone()))
}

pub(crate) fn install_playlist(host: &Rc<Playlist>) {
    PLAYLIST.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = Some(host.clone());
        }
    });
}

impl Playlist {
    pub(crate) fn new(
        ui: Weak<AppWindow>,
        state: std::rc::Weak<PlayerState>,
        session: SessionSlot,
        settings: Arc<Mutex<settings::Settings>>,
        feed: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
        browse_bvids: Arc<Mutex<Vec<String>>>,
        space_bvids: Arc<Mutex<Vec<String>>>,
    ) -> Rc<Self> {
        let restored = settings
            .lock()
            .map(|store| (store.playlist.clone(), store.playlist_current, store.play_mode))
            .unwrap_or_else(|_| (Vec::new(), None, settings::default_play_mode()));
        let (items, current) = settings::normalise_playlist_state(restored.0, restored.1);
        let mode = settings::normalise_play_mode(restored.2) as u8;
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|age| age.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let host = Rc::new(Self {
            queue: RefCell::new(PlaybackQueue {
                items: items.iter().map(QueueEntry::from_settings).collect(),
                current,
                mode,
                played: HashSet::new(),
            }),
            ui,
            state,
            session,
            settings,
            feed,
            browse_bvids,
            space_bvids,
            playing: Cell::new(false),
            stopped: Cell::new(false),
            end_handled: Cell::new(true),
            last_tick: Cell::new(Instant::now()),
            rng: Cell::new(seed | 1),
            preload_mark: RefCell::new(None),
        });
        let len = host.queue.borrow().items.len();
        if len > 0 {
            crate::debug_log(&format!(
                "playlist restore items={len} current={:?} mode={mode} autoplay=0",
                host.queue.borrow().current
            ));
        } else {
            crate::debug_log(&format!("playlist restore items=0 current=none mode={mode}"));
        }
        host.publish();
        host
    }

    /// The main window's current view (0 = 首页推荐 feed).
    pub(crate) fn view_mode(&self) -> i32 {
        self.ui
            .upgrade()
            .map(|ui| ui.get_view_mode())
            .unwrap_or(0)
    }

    pub(crate) fn mode(&self) -> u8 {
        self.queue.borrow().mode
    }

    pub(crate) fn current(&self) -> Option<usize> {
        self.queue.borrow().current
    }

    pub(crate) fn has_bvid(&self, bvid: &str) -> bool {
        self.queue.borrow().items.iter().any(|item| item.bvid == bvid)
    }

    /// Diagnostic snapshot: the queue's titles, current index and mode.
    pub(crate) fn snapshot(&self) -> (Vec<String>, Option<usize>, u8) {
        let queue = self.queue.borrow();
        (
            queue.items.iter().map(|entry| entry.bvid.clone()).collect(),
            queue.current,
            queue.mode,
        )
    }

    /// A cover for `url` if it is already decoded; `Image::default()` (none)
    /// otherwise. Never fetches - `spawn_cover` does that off-thread.
    pub(crate) fn cover_image(&self, url: &str) -> Image {
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

    /// Publish rows, the bar's now-playing block and the mode.
    ///
    /// Event-driven (enqueue/remove/clear/play/pause/end/cover), never per
    /// frame: the row model is rebuilt as a whole, which is cheap at 200 rows
    /// and keeps the `current`/`playing` flags trivially consistent.
    pub(crate) fn publish(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let playing = self.playing.get();
        let preload = prefetch::preload_snapshot();
        let (rows, current, mode, len) = {
            let queue = self.queue.borrow();
            let rows: Vec<PlaylistRow> = queue
                .items
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let current = queue.current == Some(index);
                    let (preload, preload_done) = if current {
                        // The playing item is already here; its row reads full
                        // rather than pretending a download is still pending.
                        (1.0, true)
                    } else {
                        preload_state(&preload, &entry.bvid, entry.cid)
                    };
                    PlaylistRow {
                        title: entry.title.clone().into(),
                        owner: entry.owner.clone().into(),
                        duration: entry.duration.clone().into(),
                        cover: self.cover_image(&entry.cover),
                        current,
                        playing: current && playing,
                        preload,
                        preload_done,
                    }
                })
                .collect();
            (rows, queue.current, queue.mode_value(), queue.items.len())
        };
        // The vinyl shows the FIRST playlist row by contract (`make_current`
        // keeps the playing item there), never a `now` thumbnail that can
        // outlive the queue: a close with an empty list must clear the disc
        // instead of showing the last cover forever.
        let (now_bvid, now_title, now_cover) = {
            let queue = self.queue.borrow();
            match queue.items.first() {
                Some(entry) => (
                    entry.bvid.clone(),
                    entry.title.clone(),
                    self.cover_image(&entry.cover),
                ),
                None => (String::new(), String::new(), Image::default()),
            }
        };
        ui.set_playlist(ModelRc::from(Rc::new(VecModel::from(rows))));
        ui.set_play_mode(mode);
        ui.set_now_playing(playing);
        ui.set_now_title(now_title.into());
        ui.set_now_cover(now_cover);
        self.mark_feed_queued(&ui);
        crate::debug_log(&format!(
            "playlist publish items={len} current={current:?} mode={mode} playing={playing} now={now_bvid}"
        ));
        self.preload_ahead();
    }

    /// Patch each row's preload ring **in place** on the existing 250ms tick.
    /// No timer of its own: the byte counter is cheap and the rows are a
    /// `VecModel`, so a changed ring repaints one row instead of rebuilding the
    /// list (which would also drop the hover state under the pointer).
    pub(crate) fn publish_progress(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let snapshot = prefetch::preload_snapshot();
        let model = ui.get_playlist();
        let Some(rows) = model.as_any().downcast_ref::<VecModel<PlaylistRow>>() else {
            return;
        };
        let queue = self.queue.borrow();
        for index in 0..rows.row_count().min(queue.items.len()) {
            let entry = &queue.items[index];
            let current = queue.current == Some(index);
            let (preload, done) = if current {
                (1.0, true)
            } else {
                preload_state(&snapshot, &entry.bvid, entry.cid)
            };
            let Some(mut row) = rows.row_data(index) else {
                continue;
            };
            if (row.preload - preload).abs() > 0.001 || row.preload_done != done {
                row.preload = preload;
                row.preload_done = done;
                rows.set_row_data(index, row);
            }
        }
    }

    /// Refresh the model-born `queued` flag on every home-feed card.
    ///
    /// The feed row owns the flag (not the card): a virtualised card that was
    /// recycled comes back with the check, and a row removed from the queue
    /// loses it. Only rows whose flag actually changed are written, so an
    /// enqueue repaints one card, not the list.
    pub(crate) fn mark_feed_queued(&self, ui: &AppWindow) {
        let queued: HashSet<String> = self
            .queue
            .borrow()
            .items
            .iter()
            .map(|entry| entry.bvid.clone())
            .collect();
        let rows = ui.get_feed_items();
        for index in 0..rows.row_count() {
            let Some(mut row) = rows.row_data(index) else {
                continue;
            };
            let want = queued.contains(row.bvid.as_str());
            if row.queued != want {
                row.queued = want;
                rows.set_row_data(index, row);
            }
        }
    }

    /// Preload the next playlist entries into their cache files, in order.
    ///
    /// `PrefetchHandle::preload_keys` writes exactly the file name the decoder
    /// opens for that stream, so the next item starts from local bytes instead
    /// of the network. Runs only when the queue's shape or current index
    /// changed; the prefetch handle skips keys it already holds.
    pub(crate) fn preload_ahead(&self) {
        let (current, len) = {
            let queue = self.queue.borrow();
            (queue.current, queue.items.len())
        };
        if *self.preload_mark.borrow() == Some((current, len)) {
            return;
        }
        *self.preload_mark.borrow_mut() = Some((current, len));
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let control = state
            .focused
            .borrow()
            .clone()
            .or_else(|| state.players.borrow().last().cloned())
            .map(|player| player.control.clone());
        let Some(control) = control else {
            return;
        };
        let Some(session) = active_session(&self.session) else {
            return;
        };
        let next: Vec<(String, u64)> = {
            let queue = self.queue.borrow();
            let start = queue.current.map(|index| index + 1).unwrap_or(0);
            queue
                .items
                .iter()
                .skip(start)
                .take(2)
                .map(|entry| (entry.bvid.clone(), entry.cid))
                .collect()
        };
        if next.is_empty() {
            return;
        }
        let (quality, codec) = match self.settings.lock() {
            Ok(settings) => (settings.player_quality, i64::from(settings.codec_pref)),
            Err(poisoned) => {
                let settings = poisoned.into_inner();
                (settings.player_quality, i64::from(settings.codec_pref))
            }
        };
        crate::netpool::submit(crate::netpool::Prio::Low, Box::new(move || {
            let Some(runtime) = bilibili::shared_runtime() else {
                return;
            };
            let mut order: Vec<(String, String)> = Vec::new();
            for (bvid, cid) in next {
                let Ok(info) = runtime.block_on(session.playurl(&bvid, cid)) else {
                    continue;
                };
                let qn = if quality != 0 && info.video_tracks.iter().any(|track| track.id == quality)
                {
                    quality
                } else {
                    info.default_qn
                };
                let Some(track) = player::auto_track_for(&info, qn, codec) else {
                    continue;
                };
                if track.url.is_empty() {
                    continue;
                }
                order.push((
                    format!("{bvid}-{cid}-{}-{}", track.id, track.codecid),
                    track.url.clone(),
                ));
            }
            if order.is_empty() {
                return;
            }
            crate::debug_log(&format!("playlist preload items={}", order.len()));
            let _ = slint::invoke_from_event_loop(move || {
                control.preload_playlist(&order);
            });
        }));
    }

    /// Write queue + current index + mode to `settings.json`.
    ///
    /// Written synchronously (a few KB on a user action) rather than through
    /// [`update_settings`]' off-thread save: a burst of enqueues must land in
    /// order, and a later spawned write finishing first could otherwise drop
    /// the newest items on the next launch. The file lock in `settings::save`
    /// keeps it atomic against the other writers.
    pub(crate) fn persist(&self) {
        let (snapshot, len, current, mode) = {
            let queue = self.queue.borrow();
            let mut snapshot = match self.settings.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            snapshot.playlist = queue.items.iter().map(QueueEntry::to_settings).collect();
            snapshot.playlist_current = queue.current;
            snapshot.play_mode = u32::from(queue.mode);
            (
                snapshot,
                queue.items.len(),
                queue.current,
                u32::from(queue.mode),
            )
        };
        if let Ok(mut guard) = self.settings.lock() {
            *guard = snapshot.clone();
        }
        let _ = settings::save(&snapshot);
        crate::debug_log(&format!(
            "playlist persist items={len} current={current:?} mode={mode}"
        ));
    }

    /// Publish an externally toggled play/pause immediately (the playlist
    /// header's play button). The player's 250ms tick would catch up too, but
    /// the button's icon and the row meter must answer on the click.
    pub(crate) fn set_playing(&self, playing: bool) {
        if self.playing.replace(playing) == playing {
            return;
        }
        self.publish();
    }

    /// Fetch one cover (cache miss only) and republish when it lands.
    pub(crate) fn spawn_cover(&self, url: String) {
        if url.is_empty() {
            return;
        }
        if let Ok(mut cache) = cover_cache().lock() {
            if cache.get(&url).is_some() {
                return;
            }
        }
        let Some(session) = active_session(&self.session) else {
            return;
        };
        crate::netpool::submit(crate::netpool::Prio::Low, Box::new(move || {
            let Some(runtime) = bilibili::shared_runtime() else {
                return;
            };
            let Ok(pixels) = runtime.block_on(session.fetch_thumb(&url)) else {
                crate::debug_log(&format!("playlist cover failed url={url}"));
                return;
            };
            if let Ok(mut cache) = cover_cache().lock() {
                cache.put(&url, Arc::new(pixels));
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(host) = playlist() {
                    host.publish();
                }
            });
        }));
    }

    /// Queue entry for a card index in the view that is on screen.
    ///
    /// The feed carries cid/aid itself, so it is queued synchronously. The
    /// discovery cards (分区 / 排行 / 用户空间) only carry a bvid;
    /// those are resolved through the detail API first, then queued.
    pub(crate) fn enqueue_index(&self, index: i32, only: bool) {
        let index = index.max(0) as usize;
        let view = self.view_mode();
        if view == 0 {
            let video = self
                .feed
                .lock()
                .ok()
                .and_then(|guard| guard.get(index).cloned());
            match video {
                Some(video) => self.insert_entry(QueueEntry::from_feed(&video), only),
                None => crate::debug_log(&format!(
                    "playlist enqueue miss view={view} index={index}"
                )),
            }
            return;
        }
        let bvid = match view {
            11 => self
                .space_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index).cloned()),
            8 | 9 | 10 | 18 => self
                .browse_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index).cloned()),
            _ => None,
        };
        match bvid {
            Some(bvid) => self.enqueue_bvid(bvid, only),
            None => crate::debug_log(&format!(
                "playlist enqueue miss view={view} index={index}"
            )),
        }
    }

    /// Resolve a bvid from the discovery cards into a full queue entry.
    pub(crate) fn enqueue_bvid(&self, bvid: String, only: bool) {
        if self.has_bvid(&bvid) {
            crate::debug_log(&format!("playlist dup bvid={bvid}"));
            return;
        }
        let Some(session) = active_session(&self.session) else {
            crate::debug_log(&format!("playlist enqueue skipped bvid={bvid} reason=no-session"));
            return;
        };
        let ui = self.ui.clone();
        crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
            let Some(runtime) = bilibili::shared_runtime() else {
                return;
            };
            let entry = match runtime.block_on(session.video_view(&bvid)) {
                Ok(detail) => QueueEntry {
                    bvid: detail.bvid,
                    cid: detail.cid.max(0) as u64,
                    aid: detail.aid,
                    title: detail.title,
                    owner: detail.owner_name,
                    duration: format_duration(detail.duration),
                    cover: detail.pic,
                },
                Err(err) => {
                    crate::debug_log(&format!(
                        "playlist resolve failed bvid={bvid} err={err}"
                    ));
                    return;
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if ui.upgrade().is_none() {
                    return;
                }
                if let Some(host) = playlist() {
                    host.insert_entry(entry, only);
                }
            });
        }));
    }

    /// Append one resolved entry, open the panel for a plain enqueue, and
    /// start it only when nothing is playing.
    pub(crate) fn insert_entry(&self, entry: QueueEntry, only: bool) {
        if !only {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_playlist_open(true);
            }
        }
        let bvid = entry.bvid.clone();
        if !self.queue.borrow_mut().push(entry) {
            crate::debug_log(&format!("playlist dup bvid={bvid}"));
            return;
        }
        let index = self.queue.borrow().items.len().saturating_sub(1);
        let cover = self
            .queue
            .borrow()
            .items
            .get(index)
            .map(|entry| entry.cover.clone())
            .unwrap_or_default();
        crate::debug_log(&format!(
            "playlist enqueue index={index} bvid={bvid} only={only} items={}",
            self.queue.borrow().items.len()
        ));
        self.spawn_cover(cover);
        self.publish();
        self.persist();
        self.toast_any("已加入播放列表", TOAST_KIND_SUCCESS);
        // "Nothing is playing" includes a natural stop (mode 0) and a closed
        // player; a merely paused player still owns the screen, so a new card
        // is appended without interrupting it.
        let idle = !self.playing.get() && (self.stopped.get() || !player_active());
        if !only && idle {
            self.play_index(index);
        }
    }

    /// Open the queue entry at `index` through the existing player path.
    pub(crate) fn play_index(&self, index: usize) {
        let entry = match self.queue.borrow().items.get(index).cloned() {
            Some(entry) => entry,
            None => return,
        };
        let Some(session) = active_session(&self.session) else {
            crate::debug_log(&format!(
                "playlist play skipped index={index} bvid={} reason=no-session",
                entry.bvid
            ));
            return;
        };
        let Some(state) = self.state.upgrade() else {
            return;
        };
        // The vinyl shows row 0, so playing a row also promotes it to the front.
        self.queue.borrow_mut().make_current(index);
        self.end_handled.set(false);
        self.stopped.set(false);
        crate::debug_log(&format!(
            "playlist play index={index} bvid={} title={} mode={}",
            entry.bvid,
            entry.title,
            self.mode()
        ));
        open_player_inner(
            session,
            entry.to_video(),
            &state,
            &self.settings,
            OpenMode::Replace,
            None,
        );
        self.publish();
        self.persist();
    }

    /// Remove one row. `via` names the entry point (`row-x`, `window-close`,
    /// or a test), so the log proves WHICH gesture removed the item.
    pub(crate) fn remove_via(&self, index: i32, via: &str) -> bool {
        let index = index.max(0) as usize;
        let removed = self.queue.borrow_mut().remove(index);
        let Some(removed) = removed else {
            crate::debug_log(&format!(
                "playlist remove ignored index={index} via={via}"
            ));
            return false;
        };
        crate::debug_log(&format!(
            "playlist remove index={index} bvid={} items={} current={:?} via={via}",
            removed.bvid,
            self.queue.borrow().items.len(),
            self.current()
        ));
        // The row is gone, so its preloaded bytes are dead weight. Deleting
        // before `publish` means the fresh preload pass never re-adopts them.
        prefetch::remove_cached_for(&removed.bvid, removed.cid, "playlist-remove");
        self.publish();
        self.persist();
        true
    }

    pub(crate) fn remove(&self, index: i32) {
        self.remove_via(index, "row-x");
    }

    /// A player window closed on the user's terms (X / Alt+F4): that video is
    /// done, so its queue row goes with it. A pause or an end-of-track never
    /// reaches here; a Replace close is tagged `replace` and is skipped by the
    /// caller, so auto-advance keeps the played item in the queue.
    pub(crate) fn remove_bvid_via(&self, bvid: &str, via: &str) -> bool {
        let index = self
            .queue
            .borrow()
            .items
            .iter()
            .position(|entry| entry.bvid == bvid);
        match index {
            Some(index) => self.remove_via(index as i32, via),
            None => {
                crate::debug_log(&format!(
                    "playlist remove skipped bvid={bvid} via={via} reason=not-queued"
                ));
                false
            }
        }
    }

    /// A short confirmation: the focused player's stack when one is open,
    /// otherwise the main window's own single toast (the home bar's queue mark
    /// is used with no player all the time).
    pub(crate) fn toast_any(&self, text: &str, kind: i32) {
        if let Some(state) = self.state.upgrade() {
            let player = focused_session(&state)
                .or_else(|| player_sessions(&state).last().cloned());
            if let Some(player) = player {
                toast(&player.window, text.to_string(), kind);
                crate::debug_log(&format!("playlist toast text={text} win={}", player.id));
                return;
            }
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_toast_text(text.into());
            ui.set_toast_kind(kind);
            crate::debug_log(&format!("playlist toast main=1 text={text}"));
        }
    }

    pub(crate) fn clear(&self) {
        let entries: Vec<(String, u64)> = {
            let queue = self.queue.borrow();
            queue
                .items
                .iter()
                .map(|entry| (entry.bvid.clone(), entry.cid))
                .collect()
        };
        let len = entries.len();
        self.queue.borrow_mut().clear();
        for (bvid, cid) in &entries {
            prefetch::remove_cached_for(bvid, *cid, "playlist-clear");
        }
        crate::debug_log(&format!("playlist clear removed={len}"));
        self.publish();
        self.persist();
    }

    /// Manual next/previous: queue order in every mode, wrapping at the ends.
    pub(crate) fn step(&self, forward: bool) {
        let target = self.queue.borrow().step(forward);
        if let Some(index) = target {
            crate::debug_log(&format!(
                "playlist step forward={forward} index={index} mode={}",
                self.mode()
            ));
            self.play_index(index);
        }
    }

    /// Persist the mode the UI just advanced to; never advance it again here.
    pub(crate) fn cycle_mode(&self) {
        let reported = self
            .ui
            .upgrade()
            .map(|ui| ui.get_play_mode())
            .unwrap_or_else(|| i32::from(settings::default_play_mode() as u8));
        let mode = settings::normalise_play_mode(reported.max(0) as u32);
        self.queue.borrow_mut().mode = mode as u8;
        crate::debug_log(&format!("playlist mode={mode}"));
        self.publish();
        self.persist();
    }

    /// Called by the active player's 33ms tick; self-gated to 250ms.
    pub(crate) fn tick(&self, window: &PlayerWindow, control: &player::PlaybackControl) {
        if !is_active_player(window) {
            return;
        }
        let now = Instant::now();
        if now.saturating_duration_since(self.last_tick.get()) < Duration::from_millis(250) {
            return;
        }
        self.last_tick.set(now);
        // Ring progress rides this same 250ms cadence.
        self.publish_progress();
        // A mode-0 stop only holds while the track really is over: seeking back
        // (or a fresh session) clears `track_ended`, which lets the bar follow
        // playback again instead of staying stopped forever.
        if self.stopped.get() && !control.paused() && !control.track_ended() {
            self.stopped.set(false);
        }
        // Buffering freezes the vinyl exactly like a pause: the disc follows the
        // picture, not the transport intent.
        let playing = !control.paused() && !self.stopped.get() && !control.buffering();
        if playing != self.playing.get() {
            self.playing.set(playing);
            self.publish();
        }
        if self.end_handled.get() || !control.track_ended() {
            return;
        }
        self.end_handled.set(true);
        let (mode, current, len, ended) = {
            let queue = self.queue.borrow();
            (
                queue.mode,
                queue.current,
                queue.items.len(),
                queue.current.and_then(|index| {
                    queue
                        .items
                        .get(index)
                        .map(|entry| (entry.bvid.clone(), entry.cid))
                }),
            )
        };
        crate::debug_log(&format!(
            "playlist end mode={mode} current={current:?} items={len}"
        ));
        // Finished playback owns no cache file: drop the ended item's bytes
        // now. The active player's own close path removes its session file too;
        // this also covers mode 0, where the window deliberately stays on the
        // ended frame.
        if let Some((bvid, cid)) = ended {
            prefetch::remove_cached_for(&bvid, cid, "end-of-track");
        }
        match mode {
            0 => {
                crate::debug_log("playlist end action=stop mode=0");
                self.stopped.set(true);
                self.playing.set(false);
                self.publish();
            }
            2 => {
                let (candidates, target) = {
                    let mut queue = self.queue.borrow_mut();
                    // The item that just ended counts as played; then pick a
                    // random one that has not been.
                    if let Some(current) = queue.current {
                        queue.played.insert(current);
                    }
                    let candidates = queue.random_candidates();
                    let pick = if candidates.is_empty() {
                        None
                    } else {
                        Some(self.random_below(candidates.len()))
                    };
                    let target = pick.and_then(|index| candidates.get(index).copied());
                    (candidates.len(), target)
                };
                match target {
                    Some(index) => {
                        crate::debug_log(&format!(
                            "playlist end action=shuffle candidates={candidates} pick={index}"
                        ));
                        self.play_index(index);
                    }
                    None => crate::debug_log("playlist end action=none mode=2"),
                }
            }
            _ => {
                let target = self.queue.borrow().step(true);
                match target {
                    Some(index) => {
                        crate::debug_log(&format!("playlist end action=next pick={index} mode=1"));
                        self.play_index(index);
                    }
                    None => crate::debug_log("playlist end action=none mode=1"),
                }
            }
        }
    }

    /// xorshift64: a tiny, dependency-free shuffle pick.
    pub(crate) fn random_below(&self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        let mut value = self.rng.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        self.rng.set(value);
        value ^= value >> 30;
        value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value ^= value >> 27;
        value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^= value >> 31;
        (value % bound as u64) as usize
    }

    /// A player window opened (from the queue, the feed or the detail page).
    pub(crate) fn on_open(&self, player: &PlayerWindow, video: &bilibili::FeedVideo) {
        set_active_player(player);
        // A video opened DIRECTLY (card click / detail 播放) was never in the
        // queue. Insert it as the current row now; a video opened from the
        // queue is already there and is only re-pointed at. The row is removed
        // by the window-close path, never by pause/end/replace.
        let (index, inserted) = self
            .queue
            .borrow_mut()
            .ensure_current(QueueEntry::from_feed(video));
        crate::debug_log(&format!(
            "playlist current bvid={} index={index} inserted={} via=open",
            video.bvid,
            u8::from(inserted)
        ));
        self.playing.set(true);
        self.stopped.set(false);
        self.end_handled.set(false);
        self.last_tick.set(Instant::now());
        self.spawn_cover(video.pic.clone());
        crate::debug_log(&format!(
            "playlist open bvid={} title={}",
            video.bvid, video.title
        ));
        self.persist();
        self.publish();
    }

    /// A player window closed by the user (`✕` or Alt+F4; both paths call
    /// `close_player_window`). The queue keeps its position; the bar stops
    /// reporting playback. Superseding an open never reaches here — the new
    /// [`Self::on_open`] simply takes over.
    pub(crate) fn on_close(&self) {
        clear_active_player();
        self.playing.set(false);
        self.stopped.set(true);
        self.end_handled.set(true);
        self.publish();
        crate::debug_log("playlist close playing=0");
    }

    /// Another open player window gained focus: the bar's now-playing block
    /// follows it, but the queue's position and stop/end state are kept.
    ///
    /// Closing the focused window while another one is still open lands here
    /// too, so the queue never stays attached to a window that is gone.
    pub(crate) fn on_focus(&self, player: &PlayerWindow, video: &bilibili::FeedVideo) {
        set_active_player(player);
        // The focused window's row is the marked one. `mark_current` never
        // inserts: a row the user deleted must stay deleted even if its window
        // is still open.
        let index = self.queue.borrow_mut().mark_current(&video.bvid);
        crate::debug_log(&format!(
            "playlist current bvid={} index={:?} via=focus",
            video.bvid, index
        ));
        self.last_tick.set(Instant::now());
        self.spawn_cover(video.pic.clone());
        self.persist();
        self.publish();
        crate::debug_log(&format!(
            "playlist focus bvid={} title={}",
            video.bvid, video.title
        ));
    }
}

/// Runs once per process; the timer lives in a thread-local so dropping the
/// closure cannot stop it by accident.
pub(crate) static PLAYLIST_TEST_STARTED: AtomicBool = AtomicBool::new(false);

/// Log the queue as the diagnostic run reads it.
pub(crate) fn log_playlist_state(label: &str) {
    if let Some(host) = playlist() {
        let (items, current, mode) = host.snapshot();
        crate::debug_log(&format!(
            "playlist test {label} items={} current={current:?} mode={mode} queue={items:?} now_playing={}",
            items.len(),
            host.ui
                .upgrade()
                .map(|ui| ui.get_now_playing())
                .unwrap_or(false)
        ));
    } else {
        crate::debug_log(&format!("playlist test {label} host=missing"));
    }
}

/// Seek to just before the end of the active player's track, which is the
/// "track ended" condition reached through the normal seek code path (the end
/// flag is cleared by the seek and re-latched by the player tick).
pub(crate) fn seek_near_track_end() -> bool {
    let Some(control) = player::active_control() else {
        return false;
    };
    let duration = control.duration_ms();
    if duration == 0 {
        return false;
    }
    // Two seconds of runway, not 200 ms: the final DASH segment can end a
    // little before the container duration, and a target past the last
    // decodable frame leaves the player seeking forever instead of latching
    // `track_ended`. Two seconds still reaches the end through real playback.
    let target = duration.saturating_sub(2000);
    control.seek(target);
    crate::debug_log(&format!(
        "playlist test seek duration={duration} target={target}"
    ));
    true
}

/// The strip always draws 20 cells.
pub(crate) const SCRUB_CELLS: usize = 20;

/// Mirrors of `player::DENSITY_BUCKET_MS` / `player::DENSITY_MAX_BUCKETS`, the
/// constants `compute_density` built the published curve with. Recomputing the
/// bucket width from the duration keeps the two in step without touching
/// `player.rs`.
pub(crate) const DENSITY_BUCKET_MS: u64 = 2000;

/// Bucket width `compute_density` used for a video of `duration_ms`.
pub(crate) fn density_bucket_ms(duration_ms: u64) -> u64 {
    let mut bucket = DENSITY_BUCKET_MS;
    if duration_ms / bucket + 1 > DENSITY_MAX_BUCKETS {
        bucket = (duration_ms / DENSITY_MAX_BUCKETS).max(1);
    }
    bucket
}

/// 20 heat values (0..=1) for `[base, base+span)` over the density curve.
///
/// Each density bucket contributes to the cells it overlaps, weighted by the
/// overlap length, so a bucket wider than a cell spreads instead of spiking.
/// The result is normalised to its own maximum; no data (or a non-positive
/// span) returns all zeros.
pub(crate) fn scrub_heat_values(
    buckets: &[f32],
    duration_ms: f32,
    base_ms: f32,
    span_ms: f32,
) -> Vec<f32> {
    let mut heat = vec![0.0f32; SCRUB_CELLS];
    if buckets.is_empty()
        || !base_ms.is_finite()
        || !span_ms.is_finite()
        || span_ms <= 0.0
        || !duration_ms.is_finite()
        || duration_ms <= 0.0
    {
        return heat;
    }
    let bucket_ms = density_bucket_ms(duration_ms as u64) as f64;
    let base = base_ms.max(0.0) as f64;
    let span = span_ms as f64;
    let cell = span / SCRUB_CELLS as f64;
    if cell <= 0.0 || bucket_ms <= 0.0 {
        return heat;
    }
    for (index, value) in buckets.iter().enumerate() {
        if !value.is_finite() || *value <= 0.0 {
            continue;
        }
        let bucket_start = index as f64 * bucket_ms;
        let bucket_end = bucket_start + bucket_ms;
        let start = bucket_start.max(base);
        let end = bucket_end.min(base + span);
        if end <= start {
            continue;
        }
        let mut position = start;
        while position < end {
            let cell_index =
                (((position - base) / cell).floor() as usize).min(SCRUB_CELLS - 1);
            let cell_end = base + (cell_index + 1) as f64 * cell;
            let part_end = end.min(cell_end);
            let overlap = part_end - position;
            if overlap > 0.0 {
                heat[cell_index] += (*value as f64 * (overlap / bucket_ms)) as f32;
            }
            if part_end <= position {
                break;
            }
            position = part_end;
        }
    }
    let peak = heat.iter().copied().fold(0.0f32, f32::max);
    if peak > 0.0 {
        for value in &mut heat {
            *value = (*value / peak).clamp(0.0, 1.0);
        }
    }
    heat
}

/// `20^(level-1)`, mirroring `scrub-divisor` in `ui/app.slint`.
pub(crate) fn scrub_divisor(level: i32) -> f32 {
    if level <= 1 {
        1.0
    } else if level == 2 {
        20.0
    } else if level == 3 {
        400.0
    } else if level == 4 {
        8000.0
    } else {
        160_000.0
    }
}

/// The scrub window `[base, base+span)` in ms, mirroring `scrub-base` /
/// `scrub-span` in `ui/app.slint`.
///
/// The UI's `scrub_base_ms` / `scrub_span_ms` are private Slint properties, so
/// their generated getters are not callable from Rust; the three inputs they
/// derive from (`scrub_cell_*`, `duration_ms`) ARE public and give exactly the
/// same numbers.
pub(crate) fn scrub_window(window: &PlayerWindow, level: i32) -> (f32, f32) {
    let duration = window.get_duration_ms().max(0.0);
    let base = if level >= 2 {
        window.get_scrub_cell_1() as f32 * duration / 20.0
    } else {
        0.0
    } + if level >= 3 {
        window.get_scrub_cell_2() as f32 * duration / 400.0
    } else {
        0.0
    } + if level >= 4 {
        window.get_scrub_cell_3() as f32 * duration / 8000.0
    } else {
        0.0
    } + if level >= 5 {
        window.get_scrub_cell_4() as f32 * duration / 160_000.0
    } else {
        0.0
    };
    let span = (duration / scrub_divisor(level)).max(1.0);
    (base, span)
}

/// Publish the 20 heat cells for `level` (empty model at level 0).
///
/// Reads only what the player already has: the published density curve, the
/// duration and the derived window. Runs on a zoom gesture, never per frame.
pub(crate) fn publish_scrub_heat(window: &PlayerWindow, model: &Rc<VecModel<f32>>, level: i32) {
    if level <= 0 {
        model.set_vec(Vec::new());
        crate::debug_log("scrub heat level=0 cleared");
        return;
    }
    let buckets = window.get_density_buckets();
    let values: Vec<f32> = (0..buckets.row_count())
        .filter_map(|index| buckets.row_data(index))
        .collect();
    let duration = window.get_duration_ms();
    let (base, span) = scrub_window(window, level);
    let heat = scrub_heat_values(&values, duration, base, span);
    let peak = heat.iter().copied().fold(0.0f32, f32::max);
    let list = heat
        .iter()
        .map(|value| format!("{value:.3}"))
        .collect::<Vec<_>>()
        .join(",");
    crate::debug_log(&format!(
        "scrub heat level={level} base_ms={base:.0} span_ms={span:.0} buckets={} peak={peak:.3} values=[{list}]",
        values.len()
    ));
    model.set_vec(heat);
}

/// Shared body of both player close paths (the UI's ✕ and a native close
/// request). Extracted so the two can never drift apart.
///
/// Closes exactly the window that called it: its loop, its decode session.
/// Other player windows are untouched and keep playing.
pub(crate) fn close_player_window(
    state: &Rc<PlayerState>,
    window: &Weak<PlayerWindow>,
    session: &std::rc::Weak<PlayerSession>,
    settings_store: &Arc<Mutex<settings::Settings>>,
    tag: &str,
) {
    let clock = CloseClock::new();
    let record = session.upgrade();
    let hwnd = record
        .as_ref()
        .map(|record| record.hwnd.get())
        .filter(|hwnd| *hwnd != 0)
        .or_else(|| window.upgrade().and_then(|player| win::hwnd_of(player.window())));
    // A second close (a native WM_CLOSE racing the ✕) is a no-op after the
    // first: the window is already hidden and its session retired.
    if let Some(record) = record.as_ref() {
        if record.closed.get() {
            crate::debug_log(&format!(
                "player close ignored duplicate tag={tag} win={}",
                record.id
            ));
            return;
        }
        record.closed.set(true);
    }
    // 1. The user-visible part FIRST, in this event: the window is gone before
    //    any teardown runs. Everything below is invisible work, so a slow step
    //    can no longer delay it disappearing.
    if let Some(player) = window.upgrade() {
        toast_forget(&player);
        player.set_danmaku_running(false);
        let _ = player.hide();
    }
    // The mini belongs to exactly one player: only its owner closes it.
    if let Some(hwnd) = hwnd {
        if state.mini_owner.get() == hwnd {
            if let Some(mini) = state.mini.borrow_mut().take() {
                let _ = mini.hide();
            }
            state.mini_owner.set(0);
        }
    }
    clock.hide.set(clock.ms());

    // 2. The base rect, from the still-valid player rect (a hidden window keeps
    //    its placement), so the next open restores it.
    if let (Some(record), Some(player)) = (record.as_ref(), window.upgrade()) {
        persist_player_layout(
            &record.panel_geom,
            player.window(),
            settings_store,
            record.cascade,
            "close",
        );
    }

    // 3. This window's decode session: `close()` only stores the flag and hands
    //    the prefetch proxy to a background thread (its shutdown waits, bounded,
    //    for the server task), so no ffmpeg child is waited on here.
    if let Some(record) = record.as_ref() {
        record.control.close();
        if let Some(loops) = record.loops.borrow_mut().take() {
            stop_player_loops(&loops);
        }
    }
    clock.control.set(clock.ms());

    // 4. Unregister the window. The queue follows a surviving player when the
    //    closed one was focused, and only stops when no player is left.
    let mut closed_id = 0;
    if let Some(record) = record.as_ref() {
        closed_id = record.id;
        state
            .players
            .borrow_mut()
            .retain(|open| !Rc::ptr_eq(open, record));
        let was_focused = state
            .focused
            .borrow()
            .as_ref()
            .map(|focused| Rc::ptr_eq(focused, record))
            .unwrap_or(false);
        if was_focused {
            *state.focused.borrow_mut() = None;
        }
        // `state.focused` is empty for a window that became active before its
        // native handle existed (the open path cannot register it yet). The
        // active-player slot is the proof that this close must hand over, so a
        // surviving window still gets focus — and the queue its current row.
        let was_active = is_active_player(&record.window);
        let remaining = player_sessions(state);
        if let (true, Some(next)) = (was_focused || was_active, remaining.last().cloned()) {
            focus_player(state, &next, true);
        }
        // A user close (X / native) means "I am done with this video": drop
        // its queue row. A Replace close is the auto-advance / another-video
        // path and must NOT remove anything (the queue survives an end).
        if (tag == "button" || tag == "native") && !record.live {
            if let Some(host) = playlist() {
                let closed_bvid = record.video.bvid.clone();
                host.remove_bvid_via(&closed_bvid, "window-close");
                // Focus can fail to move to a programmatically opened window
                // (or Windows can refuse activation), so the closed window may
                // never have been `focused`/`active` even though the queue had
                // already made it current. Removing its row then leaves the
                // queue detached from a player that is still alive: adopt the
                // survivor explicitly, so the vinyl keeps showing and spinning
                // the item that is actually playing.
                if !remaining.is_empty() && host.current().is_none() {
                    if let Some(next) = remaining.last() {
                        if !next.live {
                            host.on_focus(&next.window, &next.video);
                        }
                    }
                }
            }
        }
        if remaining.is_empty() {
            if let Some(host) = playlist() {
                host.on_close();
            } else {
                clear_active_player();
            }
            player::set_active_control(None);
            restore_main_min_track(state);
        }
        resume_main_window(state);
        // A USER close frees a decoder slot: the oldest queued window starts in
        // this same turn. A Replace close is followed immediately by the new
        // video's open, so the slot goes to that newest request instead.
        if tag != "replace" {
            start_next_queued(state);
        }
    }
    clock.extras.set(clock.ms());

    // 5. Keep the window object alive past this callback: dropping the last
    //    handle while Slint is executing one of its own callbacks is not safe.
    //    The deferred drop runs on a later event-loop turn.
    if let Some(record) = record {
        retire_session(record);
    }

    clock.log();
    crate::debug_log(&format!(
        "player close tag={tag} win={closed_id} remaining={}",
        state.players.borrow().len()
    ));
}

/// Start one queued window's decoder. The window already exists, so this only
/// flips its visible state and hands its stored session/video/track to the same
/// pacer every other player uses; nothing depends on which window has focus.
pub(crate) fn start_queued_playback(session: &Rc<PlayerSession>) -> bool {
    let Some(deferred) = session.deferred.borrow_mut().take() else {
        return false;
    };
    session.queued.set(false);
    let player = &session.window;
    player.set_playback_queued(false);
    player.set_playing(true);
    player.set_buffering(true);
    player.set_buffer_progress(0.0);
    session.control.set_paused(false);
    if let Some(host) = playlist() {
        host.on_open(player, &deferred.video);
    } else {
        set_active_player(player);
    }
    let video = deferred.video.clone();
    player::spawn(
        deferred.session,
        deferred.video,
        player.as_weak(),
        session.mini_slot.clone(),
        session.control.clone(),
        deferred.track,
    );
    let hwnd = session.hwnd.get();
    crate::debug_log(&format!(
        "player queued-start win={} hwnd={hwnd} bvid={} title={}",
        session.id, video.bvid, video.title
    ));
    true
}

/// Fill every free decode slot (one on a normal close) from the queued windows,
/// oldest first. Called on the UI thread from the close path.
pub(crate) fn start_next_queued(state: &Rc<PlayerState>) {
    loop {
        let running = state
            .players
            .borrow()
            .iter()
            .filter(|open| !open.queued.get())
            .count();
        if running >= MAX_CONCURRENT_PLAYERS {
            return;
        }
        let next = state
            .players
            .borrow()
            .iter()
            .find(|open| open.queued.get())
            .cloned();
        let Some(next) = next else {
            return;
        };
        if !start_queued_playback(&next) {
            return;
        }
    }
}

/// Drop a closed player window on a later event-loop turn.
///
/// The close runs inside that window's own Slint callback, so the handle must
/// outlive the callback; a zero-delay timer is enough and keeps the memory
/// (frame texture + models) from piling up across opens.
pub(crate) fn retire_session(session: Rc<PlayerSession>) {
    let id = session.id;
    slint::Timer::single_shot(Duration::from_millis(200), move || {
        drop(session);
        crate::debug_log(&format!("player retired win={id}"));
    });
}

/// Show/hide the mini (PiP) window. Playback never stops here: the mini shares
/// the running decode thread, which mirrors frames into it through `slot`.
/// Closing the mini only clears that slot.
///
/// `slot` is the slot of the player window that owns the button, so a second
/// open player's decode thread can never paint this mini.
pub(crate) fn toggle_mini(
    state: &Rc<PlayerState>,
    slot: &player::MiniSlot,
    main: &Weak<PlayerWindow>,
    control: &Arc<player::PlaybackControl>,
    title: &str,
) {
    let owner = main
        .upgrade()
        .and_then(|player| win::hwnd_of(player.window()))
        .unwrap_or(0);
    if let Some(mini) = mini_handle(state) {
        if mini.window().is_visible() {
            mini.hide().ok();
            if let Ok(mut slot) = slot.lock() {
                *slot = None;
            }
            state.mini_owner.set(0);
            if let Some(main) = main.upgrade() {
                main.set_mini_open(false);
            }
            crate::debug_log("mini hidden");
            return;
        }
        let mini_prepared = win::prepare_frameless(mini.window());
        mini.show().ok();
        if !mini_prepared {
            win::prepare_frameless(mini.window());
        }
        if let Ok(mut slot) = slot.lock() {
            *slot = Some(mini.as_weak());
        }
        state.mini_owner.set(owner);
        if let Some(main) = main.upgrade() {
            main.set_mini_open(true);
        }
        if control.topmost() {
            let _ = win::set_topmost(mini.window(), true);
        }
        crate::debug_log("mini shown again");
        return;
    }

    let mini = match MiniPlayerWindow::new() {
        Ok(mini) => mini,
        Err(err) => {
            crate::debug_log(&format!("mini create err={err}"));
            if let Some(main) = main.upgrade() {
                toast(&main, "小窗创建失败", TOAST_KIND_ERROR);
            }
            return;
        }
    };
    mini.global::<AppTheme>().set_dark(state.theme_dark.get());
    // The mini's AppTheme is its own global: mirror the main window's
    // transparency before the backdrop is published, so its `player-backdrop`
    // is the same film as home/player at first paint.
    if let Some(main) = main.upgrade() {
        mini.global::<AppTheme>()
            .set_glass_transparency(main.global::<AppTheme>().get_glass_transparency());
    }
    set_mini_backdrop(&mini, state.theme_dark.get());
    mini.set_video_title(format!("小窗 · {title}").into());
    mini.set_playing(!control.paused());
    if let Some(main) = main.upgrade() {
        mini.set_frame(main.get_frame());
        main.set_mini_open(true);
    }

    let weak = mini.as_weak();
    let mini_drag = Rc::new(win::WindowDrag::new());
    // The mini drags with the same frame-locked path as the two main windows,
    // so it needs its own 16ms tick (see the main-window wiring).
    let mini_drag_timer = Rc::new(slint::Timer::default());
    {
        let mini_weak = weak.clone();
        let drag = mini_drag.clone();
        let timer = mini_drag_timer.clone();
        mini.on_window_drag_start(move || {
            let Some(mini) = mini_weak.upgrade() else {
                return;
            };
            if let Err(reason) = drag.start(&mini.window()) {
                crate::debug_log(&format!("drag denied reason={reason} source=mini"));
                return;
            }
            crate::debug_log(&format!(
                "drag start ok=1 source={} pos={}",
                drag_source(&mini.window(), "mini"),
                drag_pos(&mini.window()),
            ));
            if timer.running() {
                return;
            }
            let tick_weak = mini_weak.clone();
            let tick_drag = drag.clone();
            let tick_timer = Rc::downgrade(&timer);
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
                    let Some(timer) = tick_timer.upgrade() else {
                        return;
                    };
                    let Some(mini) = tick_weak.upgrade() else {
                        timer.stop();
                        return;
                    };
                    if !tick_drag.is_dragging() {
                        timer.stop();
                        return;
                    }
                    if win::is_visible(&mini.window()) == Some(false) {
                        crate::debug_log("drag end reason=window-left source=mini");
                        tick_drag.end();
                        timer.stop();
                        return;
                    }
                    tick_drag.flush(&mini.window());
                    if !win::left_button_down() {
                        crate::debug_log("drag end reason=pointer-up deferred=0 source=mini");
                        tick_drag.end();
                        timer.stop();
                    }
                },
            );
        });
    }
    {
        let drag = mini_drag.clone();
        mini.on_window_drag_move(move || drag.move_to());
    }
    {
        let mini_weak = weak.clone();
        let drag = mini_drag.clone();
        let timer = mini_drag_timer.clone();
        mini.on_window_drag_end(move || {
            if let Some(mini) = mini_weak.upgrade() {
                if !drag.end_on_signal(win::left_button_down()) {
                    crate::debug_log(&format!(
                        "drag end reason=lost-grab deferred=1 source=mini {}",
                        drag_state(&mini.window())
                    ));
                    return;
                }
                timer.stop();
                drag.flush(&mini.window());
            }
            timer.stop();
            drag.end();
            crate::debug_log("drag end reason=pointer-up deferred=0 source=mini");
        });
    }
    // ---- self-driven window resize (same contract as the player) ----
    let mini_resize = Rc::new(win::WindowResize::new());
    mini_resize.set_min(320, 200);
    let mini_resize_timer = Rc::new(slint::Timer::default());
    {
        let mini_weak = weak.clone();
        let resize = mini_resize.clone();
        let timer = mini_resize_timer.clone();
        mini.on_resize_start(move |edge: i32| {
            let Some(mini) = mini_weak.upgrade() else {
                return;
            };
            if mini.window().is_maximized() {
                return;
            }
            if let Err(reason) = resize.start(&mini.window(), edge) {
                crate::debug_log(&format!(
                    "resize denied reason={reason} edge={edge} source=mini"
                ));
                return;
            }
            crate::debug_log(&format!(
                "resize start edge={edge} source=mini rect={}",
                resize_rect_text(&mini.window())
            ));
            if timer.running() {
                return;
            }
            let tick_weak = mini_weak.clone();
            let tick_resize = resize.clone();
            let tick_timer = Rc::downgrade(&timer);
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
                    let Some(timer) = tick_timer.upgrade() else {
                        return;
                    };
                    let Some(mini) = tick_weak.upgrade() else {
                        timer.stop();
                        return;
                    };
                    if !tick_resize.is_active() {
                        timer.stop();
                        return;
                    }
                    if win::is_visible(&mini.window()) == Some(false) {
                        crate::debug_log("resize end reason=window-left source=mini");
                        tick_resize.end();
                        timer.stop();
                        return;
                    }
                    tick_resize.update(&mini.window());
                    if !win::left_button_down() {
                        crate::debug_log("resize end reason=pointer-up source=mini");
                        tick_resize.end();
                        timer.stop();
                    }
                },
            );
        });
    }
    {
        let resize = mini_resize.clone();
        mini.on_resize_move(move || resize.move_to());
    }
    {
        let mini_weak = weak.clone();
        let resize = mini_resize.clone();
        let timer = mini_resize_timer.clone();
        mini.on_resize_end(move || {
            if resize.is_active() && win::left_button_down() {
                crate::debug_log("resize end reason=lost-grab deferred=1 source=mini");
                return;
            }
            if let Some(mini) = mini_weak.upgrade() {
                resize.update(&mini.window());
            }
            timer.stop();
            resize.end();
        });
    }
    {
        let control = control.clone();
        let main = main.clone();
        let mini_weak = weak.clone();
        mini.on_mini_toggle_pause(move || {
            let paused = control.toggle_paused();
            if let Some(mini) = mini_weak.upgrade() {
                mini.set_playing(!paused);
            }
            if let Some(main) = main.upgrade() {
                main.set_playing(!paused);
            }
        });
    }
    {
        let state = state.clone();
        let main = main.clone();
        let mini_weak = weak.clone();
        let slot = slot.clone();
        mini.on_mini_close(move || {
            if let Some(mini) = mini_weak.upgrade() {
                mini.hide().ok();
            }
            if let Ok(mut held) = slot.lock() {
                *held = None;
            }
            state.mini_owner.set(0);
            if let Some(main) = main.upgrade() {
                main.set_mini_open(false);
            }
            crate::debug_log("mini closed by button");
        });
    }
    {
        let mini_weak = weak.clone();
        mini.on_mini_minimize(move || {
            if let Some(mini) = mini_weak.upgrade() {
                mini.window().set_minimized(true);
            }
        });
    }
    {
        let state = state.clone();
        let main = main.clone();
        let slot = slot.clone();
        mini.window().on_close_requested(move || {
            if let Ok(mut held) = slot.lock() {
                *held = None;
            }
            state.mini_owner.set(0);
            if let Some(main) = main.upgrade() {
                main.set_mini_open(false);
            }
            crate::debug_log("mini closed by window");
            slint::CloseRequestResponse::HideWindow
        });
    }

    let mini_prepared = win::prepare_frameless(mini.window());
    if mini.show().is_err() {
        crate::debug_log("mini show failed");
        if let Some(main) = main.upgrade() {
            toast(&main, "小窗显示失败", TOAST_KIND_ERROR);
            main.set_mini_open(false);
        }
        return;
    }
    if !mini_prepared {
        win::prepare_frameless(mini.window());
    }
    // The wallpaper is anchored to the SCREEN position, so publish it again now
    // that the native window is placed (the pre-show publish had no rect).
    let (wall_x, wall_y) = publish_wallpaper(mini.global::<AppTheme>(), mini.window());
    crate::debug_log(&format!(
        "wallpaper origin win=mini stage=show x={wall_x:.1} y={wall_y:.1}"
    ));
    // Frameless mini window: the same subclass removes the non-client area.
    retry_subclass(&state);
    if let Ok(mut held) = slot.lock() {
        *held = Some(weak);
    }
    state.mini_owner.set(owner);
    if control.topmost() {
        let _ = win::set_topmost(mini.window(), true);
    }
    *state.mini.borrow_mut() = Some(mini);
    crate::debug_log("mini opened");
}

#[cfg(test)]
mod playlist_tests {
    use super::*;

    pub(crate) fn entry(bvid: &str) -> QueueEntry {
        QueueEntry {
            bvid: bvid.to_string(),
            cid: 1,
            aid: 1,
            title: bvid.to_string(),
            owner: "UP".to_string(),
            duration: "01:30".to_string(),
            cover: String::new(),
        }
    }

    pub(crate) fn queue(bvids: &[&str]) -> PlaybackQueue {
        PlaybackQueue {
            items: bvids.iter().map(|bvid| entry(bvid)).collect(),
            current: None,
            mode: 1,
            played: HashSet::new(),
        }
    }

    #[test]
    pub(crate) fn push_rejects_a_duplicate_bvid() {
        let mut queue = queue(&["BV1", "BV2"]);
        assert!(queue.push(entry("BV3")));
        assert!(!queue.push(entry("BV1")));
        assert_eq!(queue.items.len(), 3);
    }

    #[test]
    pub(crate) fn push_at_the_cap_drops_the_oldest_and_shifts_the_current() {
        let bvids: Vec<String> = (0..settings::PLAYLIST_MAX)
            .map(|index| format!("BV{index}"))
            .collect();
        let refs: Vec<&str> = bvids.iter().map(String::as_str).collect();
        let mut queue = queue(&refs);
        queue.current = Some(settings::PLAYLIST_MAX / 2);
        queue.played.insert(settings::PLAYLIST_MAX / 2 + 1);
        assert!(queue.push(entry("BVNEW")));
        assert_eq!(queue.items.len(), settings::PLAYLIST_MAX);
        assert_eq!(queue.items.first().map(|item| item.bvid.as_str()), Some("BV1"));
        assert_eq!(
            queue.items.last().map(|item| item.bvid.as_str()),
            Some("BVNEW")
        );
        assert_eq!(queue.current, Some(settings::PLAYLIST_MAX / 2 - 1));
        assert!(queue.played.contains(&(settings::PLAYLIST_MAX / 2)));
    }

    #[test]
    pub(crate) fn step_wraps_in_both_directions_in_every_mode() {
        let mut queue_value = queue(&["BV1", "BV2", "BV3"]);
        queue_value.mode = 2;
        assert_eq!(queue_value.step(true), Some(0));
        assert_eq!(queue_value.step(false), Some(2));
        queue_value.current = Some(2);
        assert_eq!(queue_value.step(true), Some(0));
        assert_eq!(queue_value.step(false), Some(1));
        let empty = queue(&[]);
        assert_eq!(empty.step(true), None);
        assert_eq!(empty.step(false), None);
    }

    #[test]
    pub(crate) fn remove_keeps_current_on_the_same_item() {
        let mut queue = queue(&["BV1", "BV2", "BV3"]);
        queue.current = Some(2);
        assert!(queue.remove(0).is_some());
        assert_eq!(queue.current, Some(1));
        assert_eq!(
            queue.items.iter().map(|item| item.bvid.as_str()).collect::<Vec<_>>(),
            vec!["BV2", "BV3"]
        );
        // Removing the current item leaves nothing current; playback itself is
        // untouched, the queue simply no longer owns it.
        assert!(queue.remove(1).is_some());
        assert_eq!(queue.current, None);
        assert!(queue.remove(5).is_none());
    }

    /// A directly opened video (card click / detail 播放) becomes the current
    /// row at index 0 even though it never went through `enqueue`; opening it
    /// again moves the existing row to the front instead of duplicating it.
    #[test]
    pub(crate) fn direct_open_inserts_one_current_row() {
        let mut queue = queue(&["BV1", "BV2"]);
        queue.current = Some(0);
        let (index, inserted) = queue.ensure_current(entry("BV3"));
        assert!(inserted);
        assert_eq!(index, 0);
        assert_eq!(queue.current, Some(0));
        assert_eq!(queue.items.len(), 3);
        assert_eq!(
            queue
                .items
                .iter()
                .map(|item| item.bvid.as_str())
                .collect::<Vec<_>>(),
            vec!["BV3", "BV1", "BV2"]
        );
        // A second open of the same video only re-points at the existing row,
        // which the contract keeps at index 0.
        queue.current = Some(2);
        let (index, inserted) = queue.ensure_current(entry("BV3"));
        assert!(!inserted);
        assert_eq!(index, 0);
        assert_eq!(queue.current, Some(0));
        assert_eq!(queue.items.len(), 3);
    }

    /// Focus marks an existing row (and promotes it to the front) but never
    /// resurrects one the user deleted.
    #[test]
    pub(crate) fn focus_marks_without_inserting() {
        let mut queue = queue(&["BV1", "BV2", "BV3"]);
        queue.current = Some(0);
        assert_eq!(queue.mark_current("BV2"), Some(0));
        assert_eq!(queue.current, Some(0));
        assert_eq!(
            queue
                .items
                .iter()
                .map(|item| item.bvid.as_str())
                .collect::<Vec<_>>(),
            vec!["BV2", "BV1", "BV3"]
        );
        assert_eq!(queue.mark_current("BV9"), None);
        assert_eq!(queue.current, Some(0));
    }

    #[test]
    pub(crate) fn clear_empties_the_queue() {
        let mut queue = queue(&["BV1"]);
        queue.current = Some(0);
        queue.played.insert(0);
        queue.clear();
        assert!(queue.items.is_empty());
        assert_eq!(queue.current, None);
        assert!(queue.played.is_empty());
    }

    #[test]
    pub(crate) fn shuffle_never_repeats_until_the_queue_is_exhausted() {
        let mut queue = queue(&["BV1", "BV2", "BV3"]);
        queue.mode = 2;
        queue.current = Some(0);
        let mut seen = HashSet::new();
        for _ in 0..3 {
            if let Some(current) = queue.current {
                queue.played.insert(current);
            }
            let candidates = queue.random_candidates();
            assert!(!candidates.is_empty());
            let pick = candidates[0];
            assert!(seen.insert(pick), "pick {pick} repeated before exhaustion");
            queue.current = Some(pick);
        }
        // Every item played: the set resets and a new round may repeat.
        if let Some(current) = queue.current {
            queue.played.insert(current);
        }
        let candidates = queue.random_candidates();
        assert!(!candidates.is_empty());
    }

    #[test]
    pub(crate) fn shuffle_with_one_item_replays_it() {
        let mut queue = queue(&["BV1"]);
        queue.current = Some(0);
        queue.played.insert(0);
        assert_eq!(queue.random_candidates(), vec![0]);
    }

    #[test]
    pub(crate) fn duration_label_parses_back_to_seconds() {
        assert_eq!(duration_seconds("01:30"), 90);
        assert_eq!(duration_seconds("1:02:03"), 3723);
        assert_eq!(duration_seconds(""), 0);
        assert_eq!(duration_seconds("bad"), 0);
    }
}
