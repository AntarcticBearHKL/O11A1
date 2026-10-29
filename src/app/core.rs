//! Shared plumbing: paths, logging, small formatters and the pieces that
//! several pages reach for (this is the `main.rs` helper layer, moved out).

use crate::*;

pub(crate) const MAX_POLLS: usize = 90;

pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Cover fetches kept in flight per list. Kept low so cover decoding (which now
/// runs on the blocking pool) never floods the CPU or the shared runtime while
/// the player is decoding video; the decoder no longer blocks the async workers.
pub(crate) const THUMB_CONCURRENCY: usize = 6;

/// Number of player windows currently open (or opening). Main-window background
/// work — incremental cover fetch/patch and the skeleton shimmer — is suspended
/// while it is non-zero so nothing competes with playback for CPU, the UI thread
/// or DWM.
///
/// A counter rather than a flag: several player windows can be open at once, so
/// closing one must not resume the main window while another still plays.
/// Bumped in `open_player_inner` and decremented on every player close path.
pub(crate) static PLAYER_OPEN_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The one live session, shared with the player worker threads.
pub(crate) type SessionSlot = Arc<Mutex<Option<Arc<bilibili::Session>>>>;

/// Region list offered by 分区浏览 (番剧/直播 excluded).
pub(crate) const REGIONS: &[(&str, i64)] = &[
    ("动画", 1),
    ("国创", 167),
    ("音乐", 3),
    ("舞蹈", 129),
    ("游戏", 4),
    ("知识", 36),
    ("科技", 188),
    ("运动", 234),
    ("汽车", 223),
    ("生活", 160),
    ("美食", 211),
    ("动物圈", 217),
    ("鬼畜", 119),
    ("时尚", 155),
    ("娱乐", 5),
    ("影视", 181),
    ("纪录片", 177),
    ("电影", 23),
    ("电视剧", 11),
];

/// The queue itself: pure data and index arithmetic, so it can be unit-tested
/// without a UI.
pub(crate) struct PlaybackQueue {
    pub(crate) items: Vec<QueueEntry>,
    pub(crate) current: Option<usize>,
    pub(crate) mode: u8,
    /// Indices shuffle mode has already auto-advanced to; reset when it covers
    /// the whole queue.
    pub(crate) played: HashSet<usize>,
}

impl Default for PlaybackQueue {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            current: None,
            mode: settings::default_play_mode() as u8,
            played: HashSet::new(),
        }
    }
}

impl PlaybackQueue {
    /// Append one entry. Returns `false` when the bvid is already queued (the
    /// same video must never occupy two rows). At the cap the OLDEST entry is
    /// dropped and every index (current, played) shifts with it.
    pub(crate) fn push(&mut self, entry: QueueEntry) -> bool {
        if self.items.iter().any(|item| item.bvid == entry.bvid) {
            return false;
        }
        if self.items.len() >= settings::PLAYLIST_MAX {
            self.items.remove(0);
            self.shift_after_drop(0);
        }
        self.items.push(entry);
        true
    }

    /// Remove the entry at `index`; current and the shuffle set follow it.
    pub(crate) fn remove(&mut self, index: usize) -> Option<QueueEntry> {
        if index >= self.items.len() {
            return None;
        }
        let removed = self.items.remove(index);
        self.shift_after_drop(index);
        Some(removed)
    }

    /// Drop the oldest entries so the queue stays inside the cap.
    pub(crate) fn shift_after_drop(&mut self, dropped: usize) {
        self.current = match self.current {
            Some(current) if current == dropped => None,
            Some(current) if current > dropped => Some(current - 1),
            other => other,
        };
        self.played = self
            .played
            .iter()
            .filter_map(|index| match (*index).cmp(&dropped) {
                std::cmp::Ordering::Less | std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(*index - 1),
            })
            .collect();
        if self.items.is_empty() {
            self.current = None;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.played.clear();
    }

    /// Make the row at `index` the current one **and move it to index 0**.
    ///
    /// The bar's vinyl always shows the first playlist row, so "currently
    /// playing" and "row 0" must be the same fact. Removing a row before the
    /// current one shifts the rest; the shuffle bookkeeping is shifted in the
    /// same pass so a later mode-2 pick still names the right entries.
    pub(crate) fn make_current(&mut self, index: usize) -> Option<usize> {
        if index >= self.items.len() {
            return None;
        }
        if index > 0 {
            let entry = self.items.remove(index);
            self.items.insert(0, entry);
            let shift = |value: usize| {
                if value == index {
                    0
                } else if value < index {
                    value + 1
                } else {
                    value
                }
            };
            self.current = self.current.map(shift);
            self.played = self.played.iter().copied().map(shift).collect();
        }
        self.current = Some(0);
        Some(0)
    }

    /// Make `entry` the current row, inserting it at the FRONT when the queue
    /// does not own it yet. This is the "a directly opened video is always in
    /// 播放列表" rule: a card click / detail 播放 never goes through `enqueue`, so
    /// `on_open` calls this with the window's video. An existing row is moved to
    /// index 0 (no duplicate). Returns `(index, inserted)`, and `index` is always
    /// 0: the currently playing item is the first queue row by contract.
    pub(crate) fn ensure_current(&mut self, entry: QueueEntry) -> (usize, bool) {
        if let Some(index) = self.items.iter().position(|item| item.bvid == entry.bvid) {
            self.make_current(index);
            return (0, false);
        }
        if self.items.len() >= settings::PLAYLIST_MAX {
            self.items.remove(0);
            self.shift_after_drop(0);
        }
        self.items.insert(0, entry);
        self.current = Some(0);
        (0, true)
    }

    /// Point `current` at an already-queued bvid and move that row to index 0.
    /// Used when another open player window gains focus: the focused row must
    /// be the marked one, but focus must never resurrect a row the user deleted.
    pub(crate) fn mark_current(&mut self, bvid: &str) -> Option<usize> {
        let index = self.items.iter().position(|item| item.bvid == bvid)?;
        self.make_current(index)
    }

    /// The next/previous index in queue order, wrapping. `None` on an empty
    /// queue; with no current item it starts at the first (next) or last
    /// (previous) entry.
    pub(crate) fn step(&self, forward: bool) -> Option<usize> {
        let len = self.items.len();
        if len == 0 {
            return None;
        }
        Some(match self.current {
            Some(current) if forward => (current + 1) % len,
            Some(current) => (current + len - 1) % len,
            None if forward => 0,
            None => len - 1,
        })
    }

    /// Indices shuffle mode may pick: everything not auto-played yet and not
    /// the item that just ended. When that set is empty the whole queue has
    /// been played, so `played` is reset and the same rule is retried; a queue
    /// with a single item then replays it, exactly like mode 1 wraps.
    pub(crate) fn random_candidates(&mut self) -> Vec<usize> {
        let pick = |queue: &PlaybackQueue| {
            (0..queue.items.len())
                .filter(|index| !queue.played.contains(index) && Some(*index) != queue.current)
                .collect::<Vec<_>>()
        };
        let mut candidates = pick(self);
        if candidates.is_empty() && !self.items.is_empty() {
            self.played.clear();
            candidates = pick(self);
            if candidates.is_empty() {
                candidates = self.step(true).into_iter().collect();
            }
        }
        candidates
    }

    /// Publish the mode as the `play_mode` property expects it.
    pub(crate) fn mode_value(&self) -> i32 {
        i32::from(self.mode)
    }
}

#[derive(Clone)]
pub(crate) struct PgcPlayerHost {
    pub(crate) state: Rc<PlayerState>,
    pub(crate) settings: Arc<Mutex<settings::Settings>>,
}

/// How many `seek requested` lines the player windows have emitted. The seek
/// drag harness samples it around one gesture: a 2s drag must add exactly 1.
pub(crate) static SEEK_REQUEST_COUNT: AtomicU64 = AtomicU64::new(0);

pub(crate) const DENSITY_MAX_BUCKETS: u64 = 400;

/// Shared handles the UI thread keeps for the open player windows.
///
/// Each window owns its playback control, panel geometry, pointer watch and
/// pointer watch; nothing here is shared between two players. `players` holds them
/// oldest first and is the only strong reference that outlives the open call
/// (the window's own callbacks use `Weak`), so removing an entry closes exactly
/// that window.
pub(crate) struct PlayerState {
    /// Every player window currently open, oldest first.
    pub(crate) players: RefCell<Vec<Rc<PlayerSession>>>,
    /// The window that last had focus (WM activation or a click). Card opens
    /// with mode 0 replace THIS one; `None` falls back to the newest.
    pub(crate) focused: RefCell<Option<Rc<PlayerSession>>>,
    /// The mini (PiP) window while one owns a real Slint window; hidden windows
    /// stay here so they can be re-shown without rebuilding.
    pub(crate) mini: RefCell<Option<MiniPlayerWindow>>,
    /// The player window the mini currently belongs to (`0` = none), so closing
    /// another player never tears down a mini it does not own.
    pub(crate) mini_owner: Cell<isize>,
    /// The main window, so a player open/close (and the player's download
    /// button) can reach it without holding a borrow.
    pub(crate) main: Weak<AppWindow>,
    /// The app's single in-flight download slot, so a download started from the
    /// player supersedes (and cancels) the previous one like the page does.
    pub(crate) download_control: Arc<Mutex<Option<Arc<download::DownloadControl>>>>,
    /// Repeating timer that retries installing the frameless subclass on the
    /// player/mini windows until their native handle exists (secondary windows
    /// are mapped late).
    pub(crate) frameless_timer: slint::Timer,
    /// Repeating timer that re-asserts the main window's frameless style, so a
    /// re-decoration that arrives without an activation message is reverted
    /// within one tick.
    pub(crate) main_frameless_timer: slint::Timer,
    /// Effective dark flag for windows created after the 主题 setting changed.
    pub(crate) theme_dark: Cell<bool>,
}

/// One open player window and everything that belongs to exactly that window.
///
/// This is the owner of the window's strong handle: a session exists while its
/// window is open, and dropping it closes the window.
pub(crate) struct PlayerSession {
    /// 1-based ordinal, so a QA log can tell two windows apart.
    pub(crate) id: u64,
    pub(crate) window: PlayerWindow,
    pub(crate) control: Arc<player::PlaybackControl>,
    pub(crate) panel_geom: Rc<RefCell<PanelGeom>>,
    /// Native handle, cached once known (identity for focus/toast).
    pub(crate) hwnd: Cell<isize>,
    /// A close path already ran for this window (a native WM_CLOSE can race
    /// the ✕); the second one is a no-op.
    pub(crate) closed: Cell<bool>,
    /// This window's 60Hz pointer watch + layout debounce.
    pub(crate) loops: RefCell<Option<Rc<PlayerLoops>>>,
    /// The decode thread mirrors frames into the mini through this slot; one
    /// per window so a background player never paints another's PiP.
    pub(crate) mini_slot: player::MiniSlot,
    /// The video this window shows, so a focus/close fallback can hand it to the
    /// queue's "now playing" block.
    pub(crate) video: bilibili::FeedVideo,
    /// Logical px this window was cascaded by when it was opened as a second
    /// window; subtracted again when persisting the base layout.
    pub(crate) cascade: i32,
    /// f150: this window is open but waiting for a decode slot (the maximum is
    /// [`MAX_CONCURRENT_PLAYERS`]). Decoding and presentation are per window;
    /// the only shared decision is how many may run at once.
    pub(crate) queued: Cell<bool>,
    /// The exact playback start the queue is holding: session + video + the
    /// window's own danmaku track. Consumed by [`start_queued_playback`].
    pub(crate) deferred: RefCell<Option<DeferredPlayback>>,
    /// A 直播 window: same window type, no `PlayInfo`, no queue row, no
    /// playlist hand-off. The flag keeps the VOD-only paths (playlist
    /// current-row, queue removal) out of a live room.
    pub(crate) live: bool,
}

/// Everything a queued player needs to start later without reopening the
/// window or refetching the video.
pub(crate) struct DeferredPlayback {
    pub(crate) session: Arc<bilibili::Session>,
    pub(crate) video: bilibili::FeedVideo,
    pub(crate) track: Arc<player::DanmakuTrack>,
}

/// How many player windows may decode/present at once. A player is a full
/// decoder + audio process + prefetch engine, so this bounds CPU and link use
/// without ever gating a running window on focus.
pub(crate) const MAX_CONCURRENT_PLAYERS: usize = 3;

/// Slint handles the native close hook reaches without capturing a `Weak`.
///
/// The hook must be `Send + Sync` and `Weak<AppWindow>` is neither, so the hook
/// itself captures nothing and reads these from the thread it fires on (the UI
/// thread, where they are installed before the window can be closed).
pub(crate) struct CloseTargets {
    pub(crate) ui: Weak<AppWindow>,
    pub(crate) store: Arc<Mutex<settings::Settings>>,
}

/// The HWND the close hook is registered for, `0` until it succeeds.
///
/// Keyed by handle rather than a plain "done" bit so a recreated window is
/// re-armed, and so the repeated retry stops as soon as it has worked once.
pub(crate) static CLOSE_HOOK_HWND: AtomicIsize = AtomicIsize::new(0);

/// Live state for the `BILI_F144_TEST` QA chain (all of it UI-thread only).
pub(crate) struct F144State {
    pub(crate) base: std::cell::RefCell<Option<Vec<u8>>>,
    pub(crate) base_wall: std::cell::Cell<(f32, f32)>,
}

/// The player chrome's pointer watch cadence: 60 Hz, the same frame cadence as
/// a 60 Hz display. One `GetCursorPos` + one `GetWindowRect` per tick.
pub(crate) const POINTER_WATCH: Duration = Duration::from_millis(16);

/// Per-drag tally of the frame-locked move timer.
///
/// `events` counts the pointer moves the drag path recorded, `placements` the
/// `SetWindowPos` calls the timer actually applied, and the rate is derived
/// from the drag's own duration when it ends. Counting here (rather than in
/// `win::WindowDrag`) keeps the log line independent of that module's private
/// profile counters.
pub(crate) struct DragFlushStats {
    pub(crate) started: Cell<Option<Instant>>,
    pub(crate) events: Cell<u64>,
    pub(crate) placements: Cell<u64>,
}

impl DragFlushStats {
    pub(crate) fn new() -> Self {
        Self {
            started: Cell::new(None),
            events: Cell::new(0),
            placements: Cell::new(0),
        }
    }

    /// Begin a drag: reset both counters and start the clock.
    pub(crate) fn begin(&self) {
        self.started.set(Some(Instant::now()));
        self.events.set(0);
        self.placements.set(0);
    }

    /// One recorded pointer move.
    pub(crate) fn event(&self) {
        self.events.set(self.events.get() + 1);
    }

    /// One applied window placement.
    pub(crate) fn placement(&self) {
        self.placements.set(self.placements.get() + 1);
    }

    /// Log the achieved rate once per drag. The end callback runs after the tick
    /// that released the button, so the second call is a no-op.
    pub(crate) fn finish(&self, source: &str) {
        let Some(started) = self.started.take() else {
            return;
        };
        let seconds = started.elapsed().as_secs_f64();
        let placements = self.placements.get();
        let hz = if seconds > 0.0 {
            placements as f64 / seconds
        } else {
            0.0
        };
        crate::debug_log(&format!(
            "drag flush hz={hz:.1} placements={placements} events={} source={source}",
            self.events.get()
        ));
    }
}

/// How long a move/resize must settle before the base rect is persisted.
pub(crate) const LAYOUT_SETTLE: Duration = Duration::from_millis(600);

/// Per-player-window UI-thread timers: the 60Hz pointer watch behind
/// `pointer_inside` and the debounced layout persistence.
///
/// One per open player, created with the window and stopped by
/// [`stop_player_loops`] on both close paths, so a closed window never keeps a
/// tick alive.
pub(crate) struct PlayerLoops {
    /// The window the timers belong to, weak so the loop never keeps it alive.
    pub(crate) window: Weak<PlayerWindow>,
    /// The owning window's ordinal, so two players' transitions are tellable
    /// apart in one log.
    pub(crate) id: u64,
    pub(crate) pointer_timer: slint::Timer,
    pub(crate) layout_timer: slint::Timer,
    /// Last `pointer_inside` pushed to the UI: only transitions are written
    /// and logged.
    pub(crate) pointer_inside: Cell<bool>,
    /// Idle-hide state for this window's chrome (cursor movement + stillness).
    pub(crate) chrome_idle: Cell<ChromeIdle>,
    /// Last window rect the layout debounce was armed for.
    pub(crate) layout_seen: Cell<Option<(i32, i32, i32, i32)>>,
    /// Last `(physical width, physical height, fullscreen)` published as the
    /// decode-target input. Only transitions are written to the control.
    pub(crate) display_seen: Cell<(u32, u32, bool)>,
    /// f152: last occlusion-grid sample, so the 60 Hz pointer tick pays the
    /// z-order walk only every [`OCCLUSION_CHECK_MS`].
    pub(crate) occlusion_checked: Cell<Option<Instant>>,
    /// Start of the current fully-covered streak; `None` while any part of the
    /// window is visible.
    pub(crate) occluded_since: Cell<Option<Instant>>,
    /// This window's picture is currently held by the occlusion watchdog.
    pub(crate) occluded_hold: Cell<bool>,
    /// When that hold was applied, for the resume line's `hidden_ms`.
    pub(crate) occluded_at: Cell<Option<Instant>>,
}

/// How often the per-window occlusion watchdog samples the z-order. Cheap
/// enough to be invisible (one 8x8 grid + a short `GetWindow` walk), and fine
/// enough to meet the 300 ms covered-before-pause rule.
pub(crate) const OCCLUSION_CHECK_MS: u64 = 100;

/// A window must be completely covered this long before the picture is held:
/// a drag/alt-tab flash must never pause the sound.
pub(crate) const OCCLUSION_PAUSE_DELAY_MS: u64 = 300;

/// f152 QA: how many times the occlusion watchdog applied/released a hold in
/// this process. Lets a side-by-side run prove "no pause" without parsing logs.
pub(crate) static OCCLUDED_PAUSES: AtomicU64 = AtomicU64::new(0);

pub(crate) static OCCLUDED_RESUMES: AtomicU64 = AtomicU64::new(0);

/// How long the pointer must be still inside the window before the chrome
/// hides (classic video-player idle behaviour).
pub(crate) const CHROME_IDLE_MS: u64 = 2500;

/// Chrome idle-hide state machine for one window.
///
/// It is pure (cursor samples in, visibility out) so the real 60 Hz watch and
/// the `BILI_IDLE_TEST` self-test drive exactly the same logic. No cursor is
/// moved and no input is synthesized anywhere.
#[derive(Clone, Copy)]
pub(crate) struct ChromeIdle {
    /// Last real cursor position seen (physical px).
    pub(crate) last_cursor: Option<(i32, i32)>,
    /// When the cursor last changed, or the window was entered.
    pub(crate) moved_at: Instant,
    /// Whether the pointer was inside on the previous sample.
    pub(crate) was_inside: bool,
    /// Whether the chrome is currently hidden by this timer.
    pub(crate) hidden: bool,
}

impl ChromeIdle {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            last_cursor: None,
            moved_at: now,
            was_inside: false,
            hidden: false,
        }
    }

    pub(crate) fn still_ms(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.moved_at).as_millis() as u64
    }

    /// Feed one sample and return whether the chrome should be visible.
    ///
    /// `held` is true while the left button is down or while the UI holds the
    /// chrome open (`chrome_hold`): both suppress the idle hide entirely.
    pub(crate) fn poll(
        &mut self,
        cursor: Option<(i32, i32)>,
        inside: bool,
        held: bool,
        now: Instant,
    ) -> bool {
        let moved = match (cursor, self.last_cursor) {
            (Some(next), Some(previous)) => next != previous,
            // The first sample has no previous position to compare with.
            (Some(_), None) => true,
            // A missing sample is not movement; a lost cursor must not pin the
            // chrome up forever.
            (None, _) => false,
        };
        if let Some(cursor) = cursor {
            self.last_cursor = Some(cursor);
        }
        // Entering the window (including a window moved under a stationary
        // cursor) is activity, so the chrome comes back. Holding the button (or
        // the UI's chrome_hold) also keeps the streak fresh, so releasing does
        // not immediately hide the chrome that was just used.
        let entered = inside && !self.was_inside;
        if moved || entered || held {
            self.moved_at = now;
        }
        self.was_inside = inside;
        self.hidden = inside && !held && self.still_ms(now) >= CHROME_IDLE_MS;
        inside && !self.hidden
    }

    /// Forget the current streak, so a hidden/closed window reopens visible.
    pub(crate) fn reset(&mut self, now: Instant) {
        self.last_cursor = None;
        self.moved_at = now;
        self.was_inside = false;
        self.hidden = false;
    }
}

/// One floating page bar button, routed by the single dispatcher.
#[derive(Clone, Copy)]
pub(crate) enum PageAction {
    First,
    Prev,
    Next,
    Refresh,
}

/// Per-view client page (0-based over the loaded data), indexed by `view_mode`.
#[derive(Default)]
pub(crate) struct ClientPages {
    pub(crate) pages: [i64; 20],
}

impl ClientPages {
    pub(crate) fn get(&self, view: i32) -> i64 {
        usize::try_from(view)
            .ok()
            .and_then(|view| self.pages.get(view))
            .copied()
            .unwrap_or(0)
            .max(0)
    }

    pub(crate) fn set(&mut self, view: i32, page: i64) {
        if let Ok(view) = usize::try_from(view) {
            if let Some(slot) = self.pages.get_mut(view) {
                *slot = page.max(0);
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum F145DragKind {
    Seek,
    Volume,
    Blur,
    Opacity,
}

impl F145DragKind {
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Seek => "seek",
            Self::Volume => "volume",
            Self::Blur => "blur",
            Self::Opacity => "opacity",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct F145DragResult {
    pub(crate) moves: u32,
    pub(crate) updates: i32,
    pub(crate) before: f32,
    pub(crate) after: f32,
    pub(crate) seek_before: u64,
    pub(crate) seek_after: u64,
}

/// The window a drag runs against. Slint components/windows are not `Clone`,
/// so the replay keeps the owned session / weak app handle and asks for the
/// window on each tick (UI thread only).
#[derive(Clone)]
pub(crate) enum F145Target {
    Player(Rc<PlayerSession>),
    App(Weak<AppWindow>),
}

impl F145Target {
    pub(crate) fn with_window<R>(&self, f: impl FnOnce(&slint::Window) -> R) -> Option<R> {
        match self {
            Self::Player(session) => Some(f(session.window.window())),
            Self::App(weak) => weak.upgrade().map(|ui| f(ui.window())),
        }
    }

    /// f151: the player's own clock, for the seek receipts.
    pub(crate) fn control(&self) -> Option<Arc<player::PlaybackControl>> {
        match self {
            Self::Player(session) => Some(session.control.clone()),
            Self::App(_) => None,
        }
    }

    /// f151: the control pill's live logical rect in window coordinates.
    pub(crate) fn pill_rect(&self) -> Option<(f32, f32, f32, f32)> {
        match self {
            Self::Player(session) => {
                let p = &session.window;
                Some((
                    p.get_qa_pill_x() as f32,
                    p.get_qa_pill_y() as f32,
                    p.get_qa_pill_w() as f32,
                    p.get_qa_pill_h() as f32,
                ))
            }
            Self::App(_) => None,
        }
    }

    /// f151: the seek band's real move counter (proves the replay's moves
    /// reached the drag handler, not just the window).
    pub(crate) fn seek_moves(&self) -> i32 {
        match self {
            Self::Player(session) => session.window.get_qa_seek_moves(),
            Self::App(_) => 0,
        }
    }

    pub(crate) fn drag_ms(&self) -> Option<f32> {
        match self {
            Self::Player(session) => Some(session.window.get_drag_ms()),
            Self::App(_) => None,
        }
    }

    pub(crate) fn dragging(&self) -> Option<bool> {
        match self {
            Self::Player(session) => Some(session.window.get_dragging()),
            Self::App(_) => None,
        }
    }
}

/// 0 = f147, 1 = f148. Read from every thread (the capture and the 4 ms probe
/// log on the UI thread), so it is an atomic rather than a thread-local.
pub(crate) static QA_WAVE: AtomicU32 = AtomicU32::new(0);

/// One UI-thread clock sample, in ms, taken by a 4ms timer. This is the same
/// thread that paints: a gap above a display frame is time a frame could not
/// be submitted. The timer is stopped before the distribution is read.
pub(crate) struct F147Probe {
    pub(crate) timer: Rc<slint::Timer>,
}

#[derive(Clone)]
pub(crate) struct F150Frame {
    pub(crate) at_ms: f64,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) rgba: Vec<u8>,
}

/// One real seek gesture's receipts. The release frame is grabbed inside the
/// event-loop turn that dispatches the Up event, before any queued frame
/// delivery can run, so it is exactly the picture the release left behind.
pub(crate) struct F151SeekCapture {
    pub(crate) drag: F145DragResult,
    pub(crate) ui_moves: i32,
    pub(crate) press_ms: f32,
    pub(crate) move_ms: f32,
    pub(crate) release_ms: f32,
    pub(crate) dragging_after: bool,
    pub(crate) scale: f64,
    pub(crate) pill: (i32, i32, i32, i32),
    pub(crate) release: F150Frame,
    pub(crate) at_100: F150Frame,
    pub(crate) frames_release: u64,
    pub(crate) frames_100: u64,
    pub(crate) frames_400: u64,
    pub(crate) freeze_release: bool,
    pub(crate) freeze_100: bool,
    pub(crate) freeze_400: bool,
    pub(crate) overlay_100: bool,
    pub(crate) overlay_400: bool,
}

/// Everything one open's clock produced. `frames` are the property-change
/// instants, i.e. one per rendered morph frame.
pub(crate) struct F152MorphStat {
    pub(crate) ticks: usize,
    pub(crate) distinct: usize,
    pub(crate) p50: f64,
    pub(crate) p95: f64,
    pub(crate) p99: f64,
    pub(crate) max: f64,
    pub(crate) span_ms: f64,
    pub(crate) last: f32,
    pub(crate) gaps: Vec<f64>,
}

/// The real [`media_hooks::MediaHooks`]: `player` owns the trace tables and
/// `prefetch` owns the queue counter, and this is the one place that knows it.
pub(crate) struct AppMediaHooks;

impl media_hooks::MediaHooks for AppMediaHooks {
    fn note_thread_role(&self, role: &'static str) {
        player::note_thread_role(role);
    }

    fn tag_here(&self, tag: &'static str) {
        player::tag_here(tag);
    }

    fn disk_queue_depth(&self) -> u64 {
        prefetch::disk_queue_depth()
    }

    fn startup_gate_enabled(&self) -> bool {
        prefetch::startup_gate_enabled()
    }
}

/// Monotonic milliseconds for the Slint morph clock. The UI language has no
/// wall clock, so AppWindow calls back here once per 8 ms tick.
pub(crate) fn mono_ms_i32() -> i32 {
    static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let ms = BASE
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(i32::MAX as u128) as i32;
    ms
}

pub(crate) fn f152_note_morph(value: f32) {
    F152_MORPH.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut() {
            frames.push((Instant::now(), value));
        }
    });
}

/// CPU time this thread has consumed (kernel + user), for the QA "UI thread
/// cores" line. `None` when the call fails.
pub(crate) fn thread_cpu_time() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    let ticks = |ft: &FILETIME| ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    Some(Duration::from_nanos((ticks(&kernel) + ticks(&user)) * 100))
}

/// Paint any window (main or player) into `path` as a PNG.
///
/// Same `PrintWindow` path as [`capture_player_png`], but the caller names the
/// file: the QA hooks that capture the MAIN window write `target/qa/<tag>.png`.
pub(crate) fn capture_window_png(window: &slint::Window, path: &std::path::Path) -> Option<(u32, u32)> {
    let (w, h, rgba) = window_rgba(window)?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let png = image::RgbaImage::from_raw(w as u32, h as u32, rgba)?;
    match png.save(path) {
        Ok(()) => {
            crate::debug_log(&format!(
                "capture png path={} size={w}x{h}",
                path.display()
            ));
            Some((w as u32, h as u32))
        }
        Err(err) => {
            crate::debug_log(&format!("capture png path={} err={err}", path.display()));
            None
        }
    }
}

/// Paint the whole window into a PNG (once per QA step).
pub(crate) fn capture_player_png(
    window: &slint::Window,
    tag: &str,
) -> Option<(u32, u32)> {
    let (w, h, rgba) = window_rgba(window)?;
    let path = std::env::temp_dir().join(format!(
        "bili_panel_test_{tag}_{}x{}.png",
        w, h
    ));
    let png = image::RgbaImage::from_raw(w as u32, h as u32, rgba)?;
    match png.save(&path) {
        Ok(()) => {
            crate::debug_log(&format!(
                "paneltest png tag={tag} path={} size={w}x{h}",
                path.display()
            ));
            Some((w as u32, h as u32))
        }
        Err(err) => {
            crate::debug_log(&format!("paneltest png tag={tag} err={err}"));
            None
        }
    }
}

/// Where a QA capture lands: `BILI_CAPTURE_DIR`, else `<cwd>/target/qa`.
pub(crate) fn qa_capture_path(tag: &str) -> std::path::PathBuf {
    let dir = std::env::var_os("BILI_CAPTURE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target").join("qa"));
    dir.join(format!("{tag}.png"))
}

/// `BILI_UP_PAGE_TEST`: wait (bounded) until page `sort` has been PUBLISHED,
/// then report how many rows the page list holds. Polling the Rust state instead
/// of the Slint model keeps the hook off the UI thread between callbacks.
pub(crate) fn up_test_wait_rows(
    up_page: &Arc<Mutex<UpPageState>>,
    up_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    sort: i32,
    limit: Duration,
) -> usize {
    let started = Instant::now();
    while started.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(200));
        let published = up_page
            .lock()
            .map(|state| state.published == Some(sort))
            .unwrap_or(false);
        if published {
            return up_store.lock().map(|page| page.len()).unwrap_or(0);
        }
    }
    0
}

/// f151: the seek freeze/swap receipts are only useful to the QA run; they are
/// enabled by the same env var that starts it.
pub(crate) fn f151_probe_enabled() -> bool {
    env::flag("BILI_F151_TEST")
}

/// The renderer line a normal run can carry too (`BILI_DEBUG_LOG` only): the
/// request line now, the loaded GPU libraries 3 s later, once the first frame
/// has created the graphics context.
pub(crate) fn f154_install_renderer_probe() {
    f154_log_renderer_boot();
    if env::var("BILI_DEBUG_LOG").is_none() {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(3000));
        f154_log(format!("renderer runtime modules={:?}", f154_gpu_modules()));
    });
}

/// Writes the five `BILI_QA_*` switches into the theme global, once, before the
/// first paint. With no variable set every property keeps its `false` default.
pub(crate) fn f154_apply_qa_flags(ui: &AppWindow) {
    let theme = ui.global::<AppTheme>();
    let flags = [
        ("BILI_QA_NO_WALLPAPER", "wallpaper"),
        ("BILI_QA_NO_GLASS", "glass"),
        ("BILI_QA_NO_SHADOW", "shadow"),
        ("BILI_QA_NO_DANMAKU", "danmaku"),
        ("BILI_QA_NO_CLIP", "clip"),
    ];
    let mut enabled: Vec<&str> = Vec::new();
    for (name, label) in flags {
        if f154_env_flag(name) {
            enabled.push(label);
        }
    }
    theme.set_qa_no_wallpaper(f154_env_flag("BILI_QA_NO_WALLPAPER"));
    theme.set_qa_no_glass(f154_env_flag("BILI_QA_NO_GLASS"));
    theme.set_qa_no_shadow(f154_env_flag("BILI_QA_NO_SHADOW"));
    theme.set_qa_no_danmaku(f154_env_flag("BILI_QA_NO_DANMAKU"));
    theme.set_qa_no_clip(f154_env_flag("BILI_QA_NO_CLIP"));
    if !enabled.is_empty() {
        f154_log(format!("qa flags on={}", enabled.join(",")));
    }
    f159_apply_card_flat(ui);
}

/// First statement of `main()`: the cold-start reference.
pub(crate) fn f159_boot_mark() {
    let _ = F159_BOOT.set(Instant::now());
}

/// `BILI_IDLE_TEST=1`: drive [`ChromeIdle`] with fake cursor samples and log
/// the four invariants (continuous movement never hides, 2.5 s of stillness
/// hides, a held button never hides, `chrome_hold` never hides). Diagnostic
/// only: nothing here touches the real cursor or any window.
pub(crate) fn spawn_idle_self_test() {
    if !env::flag("BILI_IDLE_TEST")
    {
        return;
    }
    std::thread::spawn(|| {
        let base = Instant::now();
        let mut at_ms = 0u64;
        let mut idle = ChromeIdle::new(base);
        let poll = |idle: &mut ChromeIdle,
                    at_ms: &mut u64,
                    cursor: Option<(i32, i32)>,
                    inside: bool,
                    held: bool| {
            *at_ms += 16;
            idle.poll(cursor, inside, held, base + Duration::from_millis(*at_ms))
        };

        // 1) Continuous movement (1 px every tick) for 3 s: never hides.
        let mut stayed = true;
        for step in 0..188 {
            let visible = poll(&mut idle, &mut at_ms, Some((100 + step % 2, 100)), true, false);
            stayed &= visible;
        }
        crate::debug_log(&format!(
            "idle-test movement ticks=188 hidden={} still_ms={}",
            u8::from(!stayed),
            idle.still_ms(base + Duration::from_millis(at_ms))
        ));

        // 2) Stillness: visible before the threshold, hidden after it.
        idle.reset(base + Duration::from_millis(at_ms));
        let mut before_hide = true;
        let mut hide_at = 0u64;
        for _ in 0..250 {
            let visible = poll(&mut idle, &mut at_ms, Some((100, 100)), true, false);
            let still = idle.still_ms(base + Duration::from_millis(at_ms));
            if still < CHROME_IDLE_MS {
                before_hide &= visible;
            } else if !visible && hide_at == 0 {
                hide_at = still;
            }
        }
        crate::debug_log(&format!(
            "idle-test still visible_before={} hidden={} hide_at_ms={hide_at}",
            u8::from(before_hide),
            u8::from(idle.hidden)
        ));

        // 3) Held button: 3 s of stillness must never hide.
        idle.reset(base + Duration::from_millis(at_ms));
        let mut hidden_while_held = false;
        for _ in 0..188 {
            let visible = poll(&mut idle, &mut at_ms, Some((100, 100)), true, true);
            hidden_while_held |= !visible;
        }
        crate::debug_log(&format!(
            "idle-test button held=1 hidden={}",
            u8::from(hidden_while_held)
        ));

        // 4) `chrome_hold` (popover / drag): the same hold input, must never hide.
        idle.reset(base + Duration::from_millis(at_ms));
        let mut hidden_while_hold = false;
        for _ in 0..188 {
            let visible = poll(&mut idle, &mut at_ms, Some((100, 100)), true, true);
            hidden_while_hold |= !visible;
        }
        crate::debug_log(&format!(
            "idle-test chrome_hold held=1 hidden={}",
            u8::from(hidden_while_hold)
        ));
    });
}

pub(crate) static F159_BOOT: OnceLock<Instant> = OnceLock::new();

pub(crate) fn f159_since_boot_ms() -> f64 {
    F159_BOOT
        .get()
        .map(|at| at.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// `BILI_CLOSE_PROFILE=1`: one `close profile ...` line per player close.
///
/// Every field is a completion timestamp in milliseconds since the callback's
/// first statement (`t0=0`), so the step that dominated a close is readable
/// from `BILI_DEBUG_LOG` without a profiler.
pub(crate) struct CloseClock {
    pub(crate) started: Instant,
    pub(crate) enabled: bool,
    pub(crate) hide: Cell<u128>,
    pub(crate) control: Cell<u128>,
    pub(crate) extras: Cell<u128>,
}

impl CloseClock {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            enabled: win::close_profile_enabled(),
            hide: Cell::new(0),
            control: Cell::new(0),
            extras: Cell::new(0),
        }
    }

    /// Milliseconds since entry (`0` when profiling is off, so the caller can
    /// read the clock unconditionally).
    pub(crate) fn ms(&self) -> u128 {
        if !self.enabled {
            return 0;
        }
        self.started.elapsed().as_millis()
    }

    pub(crate) fn log(&self) {
        if !self.enabled {
            return;
        }
        crate::debug_log(&format!(
            "close profile t0=0 hide={} control={} extras={} total={}",
            self.hide.get(),
            self.control.get(),
            self.extras.get(),
            self.ms(),
        ));
    }
}

/// How an open treats the players that are already up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OpenMode {
    /// Left click / queue: close the focused window and put this video there.
    Replace,
    /// Right click (card_open mode 1): a second window, leaving the others
    /// playing.
    NewWindow,
}

/// The 番剧 context a player was opened with: the season whose 选集 list fills
/// the right panel (`series_kind = 3`) and the episode currently playing.
#[derive(Clone)]
pub(crate) struct PgcPlayer {
    pub(crate) season: bilibili::PgcSeason,
    pub(crate) ep_id: i64,
}

/// Per-player 继续观看 writer state (see the danmaku tick in
/// `open_player_inner`): fires once per episode, and only after that episode has
/// a picture on screen.
pub(crate) struct PgcProgressHook {
    pub(crate) season_id: i64,
    /// The season's 正片, in 选集 order: the index is the 「第 N 集」 number.
    pub(crate) episodes: Vec<bilibili::PgcEpisode>,
    /// `control.source_seq()` last seen (changes on every 选集 switch).
    pub(crate) seq: Cell<u64>,
    /// No episode has been written since the last source change.
    pub(crate) armed: Cell<bool>,
    /// The cid already written, so a re-arm never repeats a row.
    pub(crate) written: Cell<u64>,
}

/// Smallest video band the UI allows (`PlayerWindow` min-width / min-height in
/// `ui/app.slint`). The native minimum is this plus the open strips.
pub(crate) const PLAYER_MIN_VIDEO_W: f64 = 480.0;

pub(crate) const PLAYER_MIN_VIDEO_H: f64 = 270.0;

/// A physical-pixel window rect (`GetWindowRect` / `SetWindowPos`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct WinRect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

impl WinRect {
    pub(crate) fn right(self) -> i32 {
        self.x.saturating_add(self.w)
    }

    pub(crate) fn bottom(self) -> i32 {
        self.y.saturating_add(self.h)
    }
}

impl std::fmt::Display for WinRect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{},{}x{}", self.x, self.y, self.w, self.h)
    }
}

/// Monitor bounds: `monitor` is the full screen (fullscreen detection),
/// `work` excludes the taskbar and is what panel growth is clamped to.
pub(crate) struct MonitorAreas {
    pub(crate) monitor: WinRect,
    pub(crate) work: WinRect,
}

/// Per-player panel geometry. One of these is created in `open_player_inner`
/// and captured by that player's `panel_changed` handler and danmaku tick, so
/// two player windows never share a base size.
#[derive(Default)]
pub(crate) struct PanelGeom {
    /// Panel ints last applied to the window.
    pub(crate) left: i32,
    pub(crate) right: i32,
    pub(crate) bottom: i32,
    /// Window size with NO panel open, derived at every change as
    /// `live - extras currently open` so a manual resize is respected.
    pub(crate) base_w: i32,
    pub(crate) base_h: i32,
    /// Video-area top-left with clamping NOT applied. Kept across a clamped
    /// open so closing the panel restores the exact original position.
    pub(crate) home: Option<(i32, i32)>,
    /// The rect we last applied; a live rect that differs means the user moved
    /// or resized the window, so `home` follows it.
    pub(crate) applied: Option<WinRect>,
    /// `true` while the window is fullscreen and the panels float over it.
    pub(crate) overlay: bool,
}

/// One measurement of the 收藏 page's fixed lines.
#[derive(Clone, Copy)]
pub(crate) struct F127Snap {
    pub(crate) view: i32,
    pub(crate) strip_y: f32,
    pub(crate) strip_h: f32,
    pub(crate) switch_y: f32,
    pub(crate) content_y: f32,
    pub(crate) offset: f32,
    pub(crate) tab: i32,
    pub(crate) cards: usize,
    pub(crate) empty: bool,
}

impl F127Snap {
    pub(crate) fn log(&self, tag: &str) {
        crate::debug_log(&format!(
            "f127 measure tag={tag} view={} strip_y={:.1} strip_h={:.1} switch_y={:.1} content_y={:.1} offset={:.1} fav_tab={} cards={} empty={}",
            self.view,
            self.strip_y,
            self.strip_h,
            self.switch_y,
            self.content_y,
            self.offset,
            self.tab,
            self.cards,
            u8::from(self.empty)
        ));
    }
}

/// Full-scale span of the buffer bar: `buffer_s / 2.0`, clamped 0..1. The
/// decode-ahead ring holds ~0.53 s at 30 fps, so the bar reads roughly a
/// quarter when the ring is full; 2 s is the "deep buffer" mark, chosen so
/// ordinary playback is visible instead of pinning a full bar.
pub(crate) const NET_BUFFER_FULL_SCALE_S: f32 = 2.0;

/// How often the pre-formatted strings are refreshed (the profiler itself
/// samples at 4 Hz, so faster would only repeat values).
pub(crate) const NET_PUBLISH_MS: i64 = 250;

/// One `prefetch stats` line per second (per-host split; diagnostic only).
pub(crate) const NET_SPLIT_LOG_MS: i64 = 1000;

/// `BILI_NET_TRACE=1`: how long the published slice map is logged, so a QA run
/// can see the live proxy drive the strip (never a seeded model). A millisecond
/// value (`BILI_NET_TRACE=9000`) selects a longer window.
pub(crate) const NET_TRACE_MS: i64 = 3000;

/// Cells in 下载分布: the whole file in 1/120th slices.
/// Process-wide count of 下载分布 playhead violations, read by the UI-wave QA run.
pub(crate) static MAP_CHECK_VIOLATIONS: AtomicU32 = AtomicU32::new(0);

/// Connection rows the panel lists (the proxy may run more; Rust trims).
pub(crate) const NET_WORKER_ROWS: usize = 8;

/// Samples behind the live bar: 20 x 250 ms = the last 5 s.
pub(crate) const NET_RATE_WINDOW: usize = 20;

/// 网络分析 state: the two models Rust patches in place plus the 5 s rate
/// window behind the live bar. One per player window.
pub(crate) struct NetPanel {
    /// 线程 rows, at most [`NET_WORKER_ROWS`].
    pub(crate) workers: Rc<VecModel<NetWorker>>,
    /// 下载分布, [`NET_MAP_SLICES`] cells.
    pub(crate) map: Rc<VecModel<i32>>,
    pub(crate) rate_window: RefCell<VecDeque<u64>>,
    /// `BILI_NET_TRACE`: log the published slice map for this many ms (`0` =
    /// off). `1` means the default [`NET_TRACE_MS`].
    pub(crate) trace_ms: i64,
    /// Last sample of the playhead-slice invariant, in player-tick ms.
    pub(crate) map_check_at: Cell<i64>,
    /// Position of the previous sample. A seek breaks the time->byte mapping
    /// for one tick (the new position is known before the new frame), so a
    /// discontinuity is skipped instead of counted.
    pub(crate) map_check_pos: Cell<u64>,
    /// Samples where the playhead's slice was 下载中 while playback ran at
    /// >= 28 fps. This is the release-build assertion behind the f147 line.
    pub(crate) map_violations: Cell<u32>,
}

impl NetPanel {
    pub(crate) fn new() -> Self {
        Self {
            workers: Rc::new(VecModel::from(Vec::<NetWorker>::new())),
            map: Rc::new(VecModel::from(vec![NET_MAP_MISSING; NET_MAP_SLICES])),
            rate_window: RefCell::new(VecDeque::with_capacity(NET_RATE_WINDOW)),
            trace_ms: std::env::var("BILI_NET_TRACE")
                .ok()
                .and_then(|value| match value.trim() {
                    "" => None,
                    "1" => Some(NET_TRACE_MS),
                    other => other.parse::<i64>().ok().filter(|ms| *ms > 0),
                })
                .unwrap_or(0),
            map_check_at: Cell::new(-1000),
            map_check_pos: Cell::new(0),
            map_violations: Cell::new(0),
        }
    }

    /// Hand the models to the window once; every later frame patches them.
    pub(crate) fn attach(&self, window: &PlayerWindow) {
        window.set_net_workers(ModelRc::from(self.workers.clone()));
        window.set_net_map(ModelRc::from(self.map.clone()));
    }

    /// `net_rate_ratio`: this sample over the max of the last 5 s, so the bar
    /// reads full at the session's own peak instead of a fixed scale.
    pub(crate) fn rate_ratio(&self, rate: u64) -> f32 {
        let mut window = self.rate_window.borrow_mut();
        if window.len() >= NET_RATE_WINDOW {
            window.pop_front();
        }
        window.push_back(rate);
        match window.iter().copied().max().unwrap_or(0) {
            0 => 0.0,
            peak => (rate as f32 / peak as f32).clamp(0.0, 1.0),
        }
    }

    /// Rebuild the 线程 rows, in place: `set_row_data` per row (a `set_vec`
    /// would destroy and recreate every row, which flickers on a model that
    /// changes 4x a second).
    pub(crate) fn publish_workers(&self, prefetch: Option<&prefetch::PrefetchStats>) {
        let rows: Vec<NetWorker> = prefetch
            .map(|stats| {
                stats
                    .workers
                    .iter()
                    .take(NET_WORKER_ROWS)
                    .map(|worker| NetWorker {
                        host: trim_host(&worker.host).to_string().into(),
                        range: if worker.end > worker.start {
                            format!("{}–{}", fmt_mb(worker.start), fmt_mb(worker.end)).into()
                        } else {
                            "-".into()
                        },
                        bytes: fmt_mb(worker.bytes).into(),
                        busy: worker.busy,
                    })
                    .collect()
            })
            .unwrap_or_default();
        while self.workers.row_count() > rows.len() {
            self.workers.remove(self.workers.row_count() - 1);
        }
        for (index, row) in rows.into_iter().enumerate() {
            if index < self.workers.row_count() {
                self.workers.set_row_data(index, row);
            } else {
                self.workers.push(row);
            }
        }
    }

    /// Patch 下载分布 in place and return the per-state counts (for the trace
    /// log) plus the published slice list (for the playhead check).
    /// `set_row_data`, not `set_vec`: the strip must not flicker.
    pub(crate) fn publish_map(
        &self,
        prefetch: Option<&prefetch::PrefetchStats>,
    ) -> ([u32; 4], Vec<i32>) {
        let slices = net_map_slices(prefetch);
        let mut counts = [0u32; 4];
        for (index, state) in slices.iter().enumerate() {
            counts[*state as usize] += 1;
            if self.map.row_data(index) != Some(*state) {
                self.map.set_row_data(index, *state);
            }
        }
        (counts, slices)
    }

    /// Once a second, prove the picture the user sees is not contradicted by
    /// the strip: while playback presents at least 28 fps, the slice under the
    /// playhead must be 已下载 (1), never 下载中 (3). A violation means a worker
    /// is re-fetching bytes the session already holds; it is logged with the
    /// slice index, the state and the measured fps, and counted for the QA
    /// assertion.
    pub(crate) fn check_playhead_slice(&self, stats: &player::Stats, slices: &[i32], now_ms: i64) {
        if now_ms.saturating_sub(self.map_check_at.get()) < 1000 {
            return;
        }
        self.map_check_at.set(now_ms);
        if stats.duration_ms == 0 || stats.position_ms > stats.duration_ms {
            return;
        }
        // A seek sets the new position before the frames for it arrive; the
        // old picture at 30 fps must not be read against the new playhead's
        // still-unavailable bytes. Skip exactly the discontinuous sample.
        let previous = self.map_check_pos.replace(stats.position_ms);
        let jumped = if stats.position_ms >= previous {
            stats.position_ms - previous > 3_000
        } else {
            previous - stats.position_ms > 3_000
        };
        if jumped {
            crate::debug_log(&format!(
                "map check skipped=seek from_ms={previous} position_ms={}",
                stats.position_ms
            ));
            return;
        }
        let fraction = stats.position_ms as f64 / stats.duration_ms as f64;
        let index = ((fraction * NET_MAP_SLICES as f64).floor() as usize)
            .min(NET_MAP_SLICES.saturating_sub(1));
        let state = slices.get(index).copied().unwrap_or(NET_MAP_MISSING);
        let playing = stats.fps_presented >= 28.0;
        let violated = playing && state == NET_MAP_ACTIVE;
        if violated {
            self.map_violations.set(self.map_violations.get() + 1);
            MAP_CHECK_VIOLATIONS.fetch_add(1, Ordering::SeqCst);
        }
        crate::debug_log(&format!(
            "map check playhead_slice={index} state={state} fps_presented={:.1} \
             position_ms={} duration_ms={} playing={} ok={} violations={}",
            stats.fps_presented,
            stats.position_ms,
            stats.duration_ms,
            u8::from(playing),
            u8::from(!violated),
            self.map_violations.get()
        ));
    }

    /// Publish one frame. `net_rate` is metered (no `≈`) while the loopback
    /// proxy feeds the decoder and the profiler's ESTIMATE otherwise;
    /// `net_bitrate` is always the track's declared bitrate. `now_ms` is the
    /// player tick's clock, used only by the once-a-second playhead check.
    pub(crate) fn publish(
        &self,
        window: &PlayerWindow,
        stats: Option<&player::Stats>,
        prefetch: Option<&prefetch::PrefetchStats>,
        now_ms: i64,
    ) -> [u32; 4] {
        let (counts, slices) = self.publish_map(prefetch);
        self.publish_workers(prefetch);
        let Some(stats) = stats else {
            // No track yet: clear everything so every row reads "-".
            window.set_net_fps_presented(SharedString::default());
            window.set_net_fps_decoded(SharedString::default());
            window.set_net_first_frame(SharedString::default());
            window.set_net_buffer(SharedString::default());
            window.set_net_rate(SharedString::default());
            window.set_net_bitrate(SharedString::default());
            window.set_net_progress(SharedString::default());
            window.set_net_stalls(SharedString::default());
            window.set_net_codec(SharedString::default());
            window.set_net_buffer_ratio(0.0);
            window.set_net_active_text(SharedString::default());
            window.set_net_total_text(SharedString::default());
            window.set_net_rate_ratio(0.0);
            self.rate_window.borrow_mut().clear();
            return counts;
        };
        self.check_playhead_slice(stats, &slices, now_ms);
        window.set_net_fps_presented(format!("{:.1} fps", stats.fps_presented).into());
        window.set_net_fps_decoded(format!("{:.1} fps", stats.fps_decoded).into());
        window.set_net_first_frame(
            match stats.first_frame_ms {
                Some(ms) => format!("{ms} ms"),
                None => "-".to_string(),
            }
            .into(),
        );
        window.set_net_buffer(
            format!(
                "{:.2} s ({:+.2})",
                stats.buffer_s, stats.buffer_delta_per_s
            )
            .into(),
        );
        window.set_net_rate(
            match prefetch {
                Some(metered) => fmt_socket_rate(metered.bytes_per_sec),
                None => format!("≈ {}", fmt_bps(stats.estimated_bps)),
            }
            .into(),
        );
        window.set_net_bitrate(fmt_bps(stats.declared_bps).into());
        window.set_net_progress(
            format!(
                "{} / {}",
                format_mmss(stats.position_ms),
                format_mmss(stats.duration_ms)
            )
            .into(),
        );
        window.set_net_stalls(
            format!("{} 次 · 最长 {} ms", stats.stalls, stats.longest_stall_ms).into(),
        );
        window.set_net_codec(
            if stats.width == 0 || stats.height == 0 {
                codec_name(stats.codecid).to_string()
            } else {
                format!(
                    "{} · {}×{} · {:.0} fps",
                    codec_name(stats.codecid),
                    stats.width,
                    stats.height,
                    stats.declared_fps
                )
            }
            .into(),
        );
        window.set_net_buffer_ratio((stats.buffer_s / NET_BUFFER_FULL_SCALE_S).clamp(0.0, 1.0));
        match prefetch {
            Some(metered) => {
                window.set_net_active_text(
                    format!(
                        "{} / {} 线程 · {}",
                        metered.active_workers.min(metered.workers.len() as u32),
                        metered.workers.len(),
                        fmt_socket_rate(metered.bytes_per_sec)
                    )
                    .into(),
                );
                window.set_net_total_text(net_total_text(metered).into());
                window.set_net_rate_ratio(self.rate_ratio(metered.bytes_per_sec));
            }
            None => {
                // No proxy: the worker list and the strip are honestly empty.
                window.set_net_active_text(SharedString::default());
                window.set_net_total_text(SharedString::default());
                window.set_net_rate_ratio(0.0);
                self.rate_window.borrow_mut().clear();
            }
        }
        counts
    }
}

/// One row of the published 画质 menu plus the stream it resolves to.
///
/// `spec: None` marks a locked 大会员 tier: it has no catalogue entry, so a
/// `quality_pick` on that row must not reach `set_quality_exact`.
pub(crate) struct QualityMenuItem {
    pub(crate) row: QualityMenuRow,
    pub(crate) spec: Option<usize>,
}

/// The once-per-open gate for the locked-tier toast.
///
/// A locked row is not selectable, so a click does not close the 画质 menu and
/// the same row can be hit several times in one open. The explanation is shown
/// at most once per open; every fresh open re-arms it, so the next attempt
/// after reopening the menu explains itself again.
#[derive(Default)]
pub(crate) struct LockedNotice {
    pub(crate) shown: bool,
}

impl LockedNotice {
    /// A fresh 画质 menu open: the next locked pick may show the toast.
    pub(crate) fn opened(&mut self) {
        self.shown = false;
    }

    /// True exactly once per open.
    pub(crate) fn take(&mut self) -> bool {
        !std::mem::replace(&mut self.shown, true)
    }
}

/// The stream the automatic rule selects for `qn`.
///
/// Display-only mirror of [`player::auto_track_for`]: an explicit codec
/// preference wins (highest bitrate of that codec), otherwise the **highest**
/// bitrate at that quality — the web player's 1080P AVC stream is the
/// reference, and the old lowest-bitrate rule resolved 1080P to the soft
/// ~0.58 Mbps AV1 variant. Used to mark the current row after a `qn`-only
/// quality change, where the caller never named a stream.
pub(crate) fn auto_stream_for(
    specs: &[player::QualitySpec],
    qn: i64,
    codec_pref: i64,
) -> Option<(i64, i64, u64)> {
    let stream = |spec: &player::QualitySpec| (spec.qn, spec.codecid, spec.bandwidth);
    if codec_pref != 0 {
        if let Some(spec) = specs
            .iter()
            .filter(|spec| spec.qn == qn && spec.codecid == codec_pref)
            .max_by_key(|spec| spec.bandwidth)
        {
            return Some(stream(spec));
        }
    }
    // 自动, or the preferred codec is missing at this quality: the sharpest
    // stream at the requested quality (same rule as the decoder).
    specs
        .iter()
        .filter(|spec| spec.qn == qn)
        .max_by_key(|spec| spec.bandwidth)
        .map(stream)
}

/// Sample interval for `nvidia-smi`.
pub(crate) const GPU_SAMPLE_MS: u64 = 2_000;

/// `nvidia-smi`'s own process is window-less like every other child.
pub(crate) const GPU_CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Cached utilisation + 1 (`0` = not available, so "0%" is a real reading).
pub(crate) static GPU_UTIL: AtomicU64 = AtomicU64::new(0);

/// One sampler for the whole process, however many player windows open.
pub(crate) static GPU_SAMPLER: OnceLock<()> = OnceLock::new();

/// The engine's logical space (`player.rs` VIDEO_W) and its baseline scroll
/// duration (`SCROLL_MS`). Duplicated on purpose: the fast lane extrapolates the
/// engine's motion and `danmaku_lane_tests` pins the two together.
pub(crate) const DANMAKU_VIDEO_W: f32 = 960.0;

pub(crate) const DANMAKU_SCROLL_MS: f32 = 8_000.0;

/// Budget valve: one window is 2 s of fast ticks, an average above 2.5 ms per
/// tick is too much for the UI thread, and three bad windows in a row drop the
/// lane to 60 Hz.
pub(crate) const DANMAKU_BUDGET_WINDOW: Duration = Duration::from_secs(2);

pub(crate) const DANMAKU_BUDGET_US: u64 = 2_500;

pub(crate) const DANMAKU_BUDGET_WINDOWS: u8 = 3;

/// Inter-tick gaps kept for the QA histogram (fixed size, ~34 s at 120 Hz).
pub(crate) const DANMAKU_GAP_SLOTS: usize = 4_096;

/// The QA stall: a single 200 ms sleep on the UI thread.
pub(crate) const DANMAKU_STALL_MS: u64 = 200;

/// One budget window's verdict.
pub(crate) struct BudgetWindow {
    pub(crate) avg_us: u64,
    /// Three consecutive over-budget windows: drop the lane.
    pub(crate) drop: bool,
}

/// The 2 s / 2.5 ms valve around the fast tick's own cost.
///
/// Pure state machine, so the QA self-test drives exactly what the player runs.
pub(crate) struct BudgetGuard {
    pub(crate) window_start: Instant,
    pub(crate) ticks: u32,
    pub(crate) micros: u64,
    pub(crate) bad: u8,
    pub(crate) dropped: bool,
}

impl BudgetGuard {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            ticks: 0,
            micros: 0,
            bad: 0,
            dropped: false,
        }
    }

    /// Feed one measured tick. `Some` at the end of every window.
    pub(crate) fn sample(&mut self, now: Instant, cost_us: u64) -> Option<BudgetWindow> {
        if self.ticks == 0
            && now.saturating_duration_since(self.window_start) >= DANMAKU_BUDGET_WINDOW
        {
            // The lane sat idle for a whole window (no player yet, or frozen):
            // there is no average to judge, so the window restarts here instead
            // of reporting a single tick as if it were the steady state.
            self.window_start = now;
            self.ticks = 1;
            self.micros = cost_us;
            return None;
        }
        self.ticks += 1;
        self.micros += cost_us;
        if now.saturating_duration_since(self.window_start) < DANMAKU_BUDGET_WINDOW {
            return None;
        }
        let avg_us = if self.ticks == 0 {
            0
        } else {
            self.micros / u64::from(self.ticks)
        };
        let mut drop = false;
        if avg_us > DANMAKU_BUDGET_US {
            self.bad = self.bad.saturating_add(1);
            if self.bad >= DANMAKU_BUDGET_WINDOWS && !self.dropped {
                self.dropped = true;
                drop = true;
            }
        } else {
            self.bad = 0;
        }
        self.window_start = now;
        self.ticks = 0;
        self.micros = 0;
        Some(BudgetWindow { avg_us, drop })
    }
}

/// One on-screen comment in the fast lane: the row exactly as it sits in the
/// Slint model (`item`) plus the two numbers that move it.
///
/// `base_x` is the engine's x for media time `t0_ms`, so a 滚动 comment is at
/// `base_x - speed * (t - t0_ms)` at any later time. `speed` is 0 for a
/// 顶部/底部 comment, which never moves.
pub(crate) struct FrameRow {
    pub(crate) item: DanmakuItem,
    pub(crate) base_x: f32,
    pub(crate) t0_ms: f32,
    pub(crate) speed: f32,
}

/// What one publish changed, for the once-a-second `danmaku diff` log line.
pub(crate) struct FrameDiff {
    pub(crate) updated: usize,
    pub(crate) inserted: usize,
    pub(crate) removed: usize,
    pub(crate) visible: usize,
}

/// One `BILI_F139_TEST` run's counters. Every field is a counter or a fixed-size
/// ring: starting a run allocates once and the fast tick only overwrites.
pub(crate) struct FrameQa {
    pub(crate) started: Instant,
    pub(crate) cpu_start: Option<Duration>,
    pub(crate) ticks: u64,
    pub(crate) micros: u64,
    /// Timer wakes dropped by the exact-rate gate (the 8 ms wake vs 8.33 ms
    /// interval surplus).
    pub(crate) gates: u64,
    /// Inter-tick gaps above 1.5x the target interval.
    pub(crate) late: u64,
    /// ... and above 2.5x: a frame that was really skipped.
    pub(crate) missed: u64,
    /// Rows whose x this lane rewrote, so the report can show the lane is
    /// actually moving comments instead of idling.
    pub(crate) moved: u64,
    pub(crate) gaps_us: Vec<u32>,
    pub(crate) gap_at: usize,
    pub(crate) last: Option<Instant>,
}

impl FrameQa {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            started: now,
            cpu_start: thread_cpu_time(),
            ticks: 0,
            micros: 0,
            gates: 0,
            late: 0,
            missed: 0,
            moved: 0,
            gaps_us: vec![0; DANMAKU_GAP_SLOTS],
            gap_at: 0,
            last: None,
        }
    }

    /// Record one executed tick. No allocation: the ring overwrites.
    pub(crate) fn sample(&mut self, now: Instant, cost_us: u64, interval_ms: f32, moved: usize) {
        self.ticks += 1;
        self.micros += cost_us;
        self.moved += moved as u64;
        if let Some(last) = self.last {
            let gap_us = now.saturating_duration_since(last).as_micros() as u32;
            let target_us = interval_ms * 1_000.0;
            if gap_us as f32 > target_us * 2.5 {
                self.missed += 1;
            } else if gap_us as f32 > target_us * 1.5 {
                self.late += 1;
            }
            self.gaps_us[self.gap_at] = gap_us;
            self.gap_at = (self.gap_at + 1) % self.gaps_us.len();
        }
        self.last = Some(now);
    }

    pub(crate) fn percentile(&self, percent: f32) -> u32 {
        let mut gaps: Vec<u32> = self.gaps_us.iter().copied().filter(|gap| *gap > 0).collect();
        if gaps.is_empty() {
            return 0;
        }
        gaps.sort_unstable();
        let index = ((gaps.len() - 1) as f32 * percent / 100.0).round() as usize;
        gaps[index.min(gaps.len() - 1)]
    }

    pub(crate) fn wall(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.started).as_secs_f32()
    }
}

/// The one-shot QA stall probe.
pub(crate) struct StallProbe {
    pub(crate) due: Instant,
    pub(crate) slept: bool,
    /// Frames still to log after the stall, so the catch-up is visible.
    pub(crate) follow: u8,
}

/// The danmaku layer's fast half: the Slint model, the on-screen rows and the
/// motion clock. One per player window, UI thread only.
pub(crate) struct DanmakuLayer {
    pub(crate) window: Weak<PlayerWindow>,
    pub(crate) model: Rc<VecModel<DanmakuItem>>,
    pub(crate) inner: RefCell<LayerInner>,
}

pub(crate) struct LayerInner {
    pub(crate) rows: Vec<FrameRow>,
    /// Media time (ms) and the monotonic instant the slow tick sampled it at.
    pub(crate) anchor: Option<(f32, Instant)>,
    /// Media time the comments are currently drawn at.
    pub(crate) t_ms: f32,
    /// Earliest instant the next fast frame may run (the exact-rate gate).
    pub(crate) due: Option<Instant>,
    /// Set while nothing may move: paused, buffering or 弹幕 off.
    pub(crate) frozen: bool,
    pub(crate) hz: u32,
    pub(crate) interval_ms: f32,
    pub(crate) speed_percent: u32,
    /// Media position of the previous publish, so a frozen clock is not read as
    /// "the comment did not move" when classifying 顶部/底部 rows.
    pub(crate) last_publish_ms: f32,
    /// QA only (`BILI_F139_SLOW_MS`): make every fast tick cost this many
    /// milliseconds, so the budget valve can be exercised end to end.
    pub(crate) slow_ms: u64,
    pub(crate) budget: BudgetGuard,
    pub(crate) qa: Option<FrameQa>,
    pub(crate) stall: Option<StallProbe>,
}

impl DanmakuLayer {
    pub(crate) fn new(window: Weak<PlayerWindow>, model: Rc<VecModel<DanmakuItem>>, hz: u32) -> Self {
        Self {
            window,
            model,
            inner: RefCell::new(LayerInner {
                rows: Vec::new(),
                anchor: None,
                t_ms: 0.0,
                due: None,
                frozen: true,
                hz,
                interval_ms: danmaku_frame_interval_ms(hz),
                speed_percent: 100,
                last_publish_ms: -1.0,
                slow_ms: 0,
                budget: BudgetGuard::new(Instant::now()),
                qa: None,
                stall: None,
            }),
        }
    }

    /// Stop the motion clock while paused / buffering / 弹幕 off. The next
    /// publish unfreezes it with a fresh anchor.
    pub(crate) fn freeze(&self) {
        self.inner.borrow_mut().frozen = true;
    }

    pub(crate) fn hz(&self) -> u32 {
        self.inner.borrow().hz
    }

    /// Push a new rate to the timer and restart the gate schedule.
    pub(crate) fn apply_hz(&self, hz: u32) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.hz = hz;
            inner.interval_ms = danmaku_frame_interval_ms(hz);
            inner.due = None;
        }
        if let Some(window) = self.window.upgrade() {
            window.set_danmaku_frame_interval(danmaku_wake_ms(hz));
        }
    }

    /// The slow tick's half: reconcile the model with a fresh
    /// `track.compute()` result and re-anchor the motion clock.
    ///
    /// Everything expensive lives here on purpose - the track's lock, the item
    /// vector, the string comparisons - at 30 Hz, not 120.
    pub(crate) fn publish(
        &self,
        position_ms: f32,
        items: Vec<DanmakuItem>,
        opacity: f32,
        speed_percent: u32,
        moving: bool,
        now: Instant,
    ) -> FrameDiff {
        let mut inner = self.inner.borrow_mut();
        let advanced = position_ms - inner.last_publish_ms >= 1.0;
        inner.last_publish_ms = position_ms;
        let resync = inner.anchor.is_none()
            || (position_ms - inner.t_ms).abs() > DANMAKU_FRAME_RESYNC_MS;
        if resync {
            inner.t_ms = position_ms;
        }
        inner.anchor = Some((position_ms, now));
        inner.frozen = !moving;
        let speed_changed = inner.speed_percent != speed_percent;
        inner.speed_percent = speed_percent;
        let inner = &mut *inner;

        let mut diff = FrameDiff {
            updated: 0,
            inserted: 0,
            removed: 0,
            visible: 0,
        };
        // Rows whose dmid left the visible set go away; iterate backwards so the
        // earlier indexes stay valid.
        let mut index = inner.rows.len();
        while index > 0 {
            index -= 1;
            if !items
                .iter()
                .any(|item| item.dmid.as_str() == inner.rows[index].item.dmid.as_str())
            {
                self.model.remove(index);
                inner.rows.remove(index);
                diff.removed += 1;
            }
        }
        for item in items {
            // Bake the layer opacity into the colour alpha: a group opacity on
            // the danmaku layer would force an intermediate composition layer
            // on every frame (see ui/app.slint).
            let mut item = item;
            item.color = item.color.with_alpha(opacity);
            match inner
                .rows
                .iter()
                .position(|row| row.item.dmid == item.dmid)
            {
                Some(index) => {
                    let row = &mut inner.rows[index];
                    let engine_x = item.x;
                    // The engine's x moved since the last publish, so this row
                    // is a 滚动 comment; a centred row that the clock did not
                    // advance is the 顶部/底部 case. Only evaluated while the
                    // media clock actually advanced, so a buffering publish
                    // cannot misclassify a scroll row as static.
                    let width_changed = (item.w - row.item.w).abs() > 0.01;
                    let classify = width_changed || speed_changed;
                    if classify {
                        row.speed = if danmaku_is_centred(engine_x, item.w) {
                            0.0
                        } else {
                            danmaku_scroll_speed(item.w, speed_percent)
                        };
                    } else if row.speed == 0.0
                        && advanced
                        && (engine_x - row.base_x).abs() >= 1.0
                    {
                        // A centred row that keeps moving is a 滚动 comment
                        // caught at the centre when it appeared.
                        row.speed = danmaku_scroll_speed(item.w, speed_percent);
                    }
                    let mut changed = false;
                    if row.item.y != item.y {
                        row.item.y = item.y;
                        changed = true;
                    }
                    if row.item.w != item.w {
                        row.item.w = item.w;
                        changed = true;
                    }
                    if row.item.text != item.text {
                        row.item.text = item.text;
                        changed = true;
                    }
                    if row.item.color != item.color {
                        row.item.color = item.color;
                        changed = true;
                    }
                    if row.item.size != item.size {
                        row.item.size = item.size;
                        changed = true;
                    }
                    if row.item.bold != item.bold {
                        row.item.bold = item.bold;
                        changed = true;
                    }
                    if row.item.mid_hash != item.mid_hash {
                        row.item.mid_hash = item.mid_hash;
                        changed = true;
                    }
                    // A static row is placed by the engine; a moving one by the
                    // fast lane, which is why its x is left alone here.
                    if row.speed == 0.0 || classify {
                        if row.item.x != engine_x {
                            row.item.x = engine_x;
                            changed = true;
                        }
                        row.base_x = engine_x;
                        row.t0_ms = position_ms;
                    }
                    if changed {
                        self.model.set_row_data(index, row.item.clone());
                        diff.updated += 1;
                    }
                }
                None => {
                    let speed = if danmaku_is_centred(item.x, item.w) {
                        0.0
                    } else {
                        danmaku_scroll_speed(item.w, speed_percent)
                    };
                    let row = FrameRow {
                        base_x: item.x,
                        t0_ms: position_ms,
                        speed,
                        item,
                    };
                    self.model.push(row.item.clone());
                    inner.rows.push(row);
                    diff.inserted += 1;
                }
            }
        }
        diff.visible = inner.rows.len();
        debug_assert_eq!(diff.visible, self.model.row_count());
        diff
    }

    /// One fast frame: advance the motion clock and rewrite the x of the
    /// comments already on screen.
    ///
    /// No lock, no allocation, no formatting, no property write beyond the model
    /// rows whose whole pixel changed.
    pub(crate) fn frame_tick(&self) {
        self.tick(Instant::now());
    }

    /// [`frame_tick`](Self::frame_tick) with an explicit instant, so the QA
    /// self-tests drive the very same code with a synthetic clock.
    pub(crate) fn tick(&self, now: Instant) {
        let frame_start = Instant::now();
        let mut drop_to = None;
        let mut budget_log = None;
        {
            let mut inner = self.inner.borrow_mut();
            if inner.frozen {
                return;
            }
            // The exact-rate gate: Slint durations are whole milliseconds, so
            // 120 Hz wakes every 8 ms. A wake that arrives before the schedule's
            // next slot is dropped instead of running the lane 4 % fast.
            if let Some(due) = inner.due {
                if now < due {
                    if let Some(qa) = inner.qa.as_mut() {
                        qa.gates += 1;
                    }
                    return;
                }
            }
            let Some((anchor_ms, anchor_at)) = inner.anchor else {
                return;
            };
            // QA: burn the budget on purpose so the valve trips for real.
            if inner.slow_ms > 0 {
                std::thread::sleep(Duration::from_millis(inner.slow_ms));
            }
            // QA: one 200 ms sleep on the UI thread, so the report can show what
            // the motion clock does with a stall.
            let mut stall_begin = None;
            if let Some(probe) = inner.stall.as_mut() {
                if !probe.slept && now >= probe.due {
                    probe.slept = true;
                    probe.follow = 3;
                    stall_begin = Some(inner.t_ms);
                    crate::debug_log(&format!(
                        "danmaku stall begin sleep_ms={DANMAKU_STALL_MS} t_ms={:.1}",
                        inner.t_ms
                    ));
                    std::thread::sleep(Duration::from_millis(DANMAKU_STALL_MS));
                }
            }
            // The motion clock: the slow tick's media position plus the
            // monotonic time since it sampled that position. Nothing is carried
            // over from the previous frame, so a late or missed tick cannot
            // accumulate drift.
            let want = anchor_ms + now.saturating_duration_since(anchor_at).as_secs_f32() * 1_000.0;
            let step = (want - inner.t_ms).min(DANMAKU_FRAME_MAX_STEP_MS);
            let t = if want <= inner.t_ms {
                want
            } else {
                inner.t_ms + step
            };
            inner.t_ms = t;
            if let Some(before_t) = stall_begin {
                crate::debug_log(&format!(
                    "danmaku stall resume wall_elapsed_ms={:.1} want_t_ms={want:.1} \
                     used_t_ms={t:.1} step_ms={:.1} capped={}",
                    now.saturating_duration_since(anchor_at)
                        .as_secs_f32()
                        * 1_000.0,
                    t - before_t,
                    u8::from(want - before_t > DANMAKU_FRAME_MAX_STEP_MS)
                ));
            } else if let Some(probe) = inner.stall.as_mut() {
                // The frames after the stall: each one is capped, and the media
                // time closes on the anchor instead of overshooting it.
                if probe.follow > 0 {
                    probe.follow -= 1;
                    crate::debug_log(&format!(
                        "danmaku stall catchup n={} t_ms={t:.1} want_t_ms={want:.1} \
                         step_ms={step:.1} capped={} lag_ms={:.1}",
                        3 - probe.follow,
                        u8::from(step >= DANMAKU_FRAME_MAX_STEP_MS),
                        want - t
                    ));
                }
            }
            // Whole-pixel x: a sub-pixel offset changes the glyph phase on every
            // frame, which makes the renderer re-shape the text (see
            // `player.rs::active_items`) - and at 120 Hz that is the whole cost.
            let mut moved = 0usize;
            for (index, row) in inner.rows.iter_mut().enumerate() {
                if row.speed == 0.0 {
                    continue;
                }
                let x = (row.base_x - row.speed * (t - row.t0_ms)).round();
                if x == row.item.x {
                    continue;
                }
                row.item.x = x;
                self.model.set_row_data(index, row.item.clone());
                moved += 1;
            }
            // Budget valve: this tick's own cost, averaged per 2 s window. The
            // QA stall's 200 ms sleep is not the lane's cost, so it is skipped.
            let cost_us = frame_start.elapsed().as_micros() as u64;
            let window = if stall_begin.is_some() {
                None
            } else {
                inner.budget.sample(now, cost_us)
            };
            let qa_on = inner.qa.is_some();
            if let Some(window) = window {
                if window.drop {
                    drop_to = Some((inner.hz, 60u32));
                }
                if qa_on || window.avg_us > DANMAKU_BUDGET_US {
                    budget_log = Some(window.avg_us);
                }
            }
            let interval_ms = inner.interval_ms;
            if let Some(qa) = inner.qa.as_mut() {
                qa.sample(now, cost_us, interval_ms, moved);
            }
            // Fixed-rate schedule, one executed tick per target interval. A tick
            // that is more than one interval late restarts the schedule instead
            // of replaying the slots it missed.
            let interval = Duration::from_secs_f32(interval_ms / 1_000.0);
            inner.due = Some(match inner.due {
                Some(due) if now.saturating_duration_since(due) <= interval => due + interval,
                _ => now + interval,
            });
        }
        if let Some(avg_us) = budget_log {
            crate::debug_log(&format!(
                "danmaku frame budget hz={} window_avg_us={avg_us}",
                self.hz()
            ));
        }
        if let Some((from, to)) = drop_to {
            if to < from {
                self.apply_hz(to);
                crate::debug_log(&format!("danmaku hz {from}→{to} reason=budget"));
            }
        }
    }

    /// Begin a QA measurement run; `stall_at` arms the one-shot sleep.
    pub(crate) fn start_qa(&self, now: Instant, stall_at: Option<Instant>) {
        let mut inner = self.inner.borrow_mut();
        // `BILI_F139_SLOW_MS`: inflate the per-tick cost so the 2 s / 2.5 ms
        // valve has something to trip over (QA only; 0 keeps the real cost).
        inner.slow_ms = std::env::var("BILI_F139_SLOW_MS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(0)
            .min(50);
        inner.qa = Some(FrameQa::new(now));
        inner.stall = stall_at.map(|due| StallProbe {
            due,
            slept: false,
            follow: 0,
        });
    }

    /// End the run: log the table and return the rate it ran at.
    pub(crate) fn finish_qa(&self, now: Instant) -> u32 {
        let mut inner = self.inner.borrow_mut();
        let hz = inner.hz;
        let rows = inner.rows.len();
        let Some(qa) = inner.qa.take() else {
            return hz;
        };
        let wall = qa.wall(now).max(0.001);
        let ticks_per_s = qa.ticks as f32 / wall;
        let mean_us = if qa.ticks == 0 {
            0
        } else {
            qa.micros / qa.ticks
        };
        let cores = match (qa.cpu_start, thread_cpu_time()) {
            (Some(start), Some(end)) => end.saturating_sub(start).as_secs_f32() / wall,
            _ => -1.0,
        };
        crate::debug_log(&format!(
            "danmaku frame qa hz={hz} ticks={} ticks_per_s={ticks_per_s:.1} \
             mean_us={mean_us} moved_per_tick={:.1} gap_p50_us={} gap_p95_us={} \
             gap_p99_us={} gap_max_us={} late={} missed={} gates={} rows={rows} \
             ui_thread_cores={cores:.3} interval_ms={:.2} display_hz={}",
            qa.ticks,
            if qa.ticks == 0 {
                0.0
            } else {
                qa.moved as f32 / qa.ticks as f32
            },
            qa.percentile(50.0),
            qa.percentile(95.0),
            qa.percentile(99.0),
            qa.percentile(100.0),
            qa.late,
            qa.missed,
            qa.gates,
            inner.interval_ms,
            display_refresh_hz()
        ));
        inner.stall = None;
        inner.slow_ms = 0;
        hz
    }
}

/// One pickable row of a player's 选集 list.
///
/// `None` in [`SeriesState::targets`] marks a section header row: it occupies a
/// model slot (so the flat index the UI reports is the same one Rust resolves)
/// but can never be picked.
#[derive(Clone, Debug, Default)]
pub(crate) struct SeriesTarget {
    pub(crate) bvid: String,
    pub(crate) cid: u64,
    pub(crate) aid: i64,
    pub(crate) title: String,
    /// PGC only: the episode's own `ep_id`, used to gate a switch before it is
    /// allowed to replace the playing stream.
    pub(crate) ep_id: i64,
}

/// The 选集 model of ONE player window. Filled once from `video_view` at open
/// and republished in place when an episode is picked.
#[derive(Clone, Debug, Default)]
pub(crate) struct SeriesState {
    /// 0 none · 1 分P · 2 合集 (`ugc_season`).
    pub(crate) kind: i32,
    pub(crate) title: String,
    pub(crate) count: i32,
    pub(crate) rows: Vec<SeriesRow>,
    /// Parallel to `rows` (`None` = header).
    pub(crate) targets: Vec<Option<SeriesTarget>>,
}

/// Publish a completed [`SeriesState`] to one player window.
///
/// UI-THREAD ONLY: it writes window properties and arms a `slint::Timer`. The
/// 分P / 合集 fetch runs on the network pool, where `Weak::upgrade()` returns
/// `None` for a UI-thread component — that silent gate is exactly why a multi-P
/// video's 选集 button stayed dim (F157/F158). Callers on a worker thread must
/// hop through `slint::invoke_from_event_loop` first.
pub(crate) fn publish_series_state(
    weak: &Weak<PlayerWindow>,
    series_state: &Arc<Mutex<SeriesState>>,
    state: SeriesState,
) {
    let Some(window) = weak.upgrade() else {
        // Never silent: a publish that cannot reach its window is the failure
        // mode this gate exists to expose.
        crate::debug_log(&format!(
            "series publish skipped kind={} rows={} reason=window-gone-or-wrong-thread",
            state.kind,
            state.rows.len()
        ));
        return;
    };
    let scroll = series_scroll_for(&state);
    if let Ok(mut slot) = series_state.lock() {
        *slot = state.clone();
    }
    window.set_series_kind(state.kind);
    window.set_series_title(state.title.clone().into());
    window.set_series_count(state.count);
    // Readback: a publish that does not land on the window that is on screen
    // (the 选集 cell stays dim) must be visible in the log, not inferred.
    crate::debug_log(&format!(
        "series publish kind={} window_kind={} rows={}",
        state.kind,
        window.get_series_kind(),
        state.rows.len()
    ));
    window.set_series_rows(ModelRc::from(Rc::new(VecModel::from(
        state.rows.clone(),
    ))));
    window.set_series_scroll(scroll);
    // The list body only exists while the 选集 tab is open: a scroll offset
    // written before that is clamped away by the freshly created ScrollView, so
    // re-apply it once it is laid out (only when the panel is on the tab).
    if window.get_right_panel() != 0 && window.get_right_tab() == 4 {
        let weak = window.as_weak();
        slint::Timer::single_shot(Duration::from_millis(120), move || {
            if let Some(window) = weak.upgrade() {
                window.set_series_scroll(scroll);
            }
        });
    }
}

#[derive(Default)]
pub(crate) struct LiveState {
    pub(crate) modules: Vec<bilibili::LiveModule>,
    pub(crate) areas: Vec<bilibili::LiveArea>,
    /// 0 = 推荐; otherwise the chip index (`areas[sel - 1]`).
    pub(crate) area_sel: i32,
    /// The rooms currently published as cards, in model order. Click routing
    /// and the room player both read from here.
    pub(crate) rooms: Vec<bilibili::LiveRoom>,
    pub(crate) page: i64,
    pub(crate) has_more: bool,
    pub(crate) loading: bool,
    /// Non-empty = the B站 risk-control sentence for this area.
    pub(crate) risk: String,
}

/// Send + Sync page state shared by the workers and the event-loop closures.
/// The two models are NOT held here: they are published through the window
/// (`get_live_cards` / `set_live_cards`) on the UI thread, like the 番剧 grid,
/// because a model handle cannot cross a thread boundary.
#[derive(Clone)]
pub(crate) struct LiveHost {
    pub(crate) state: Arc<Mutex<LiveState>>,
    pub(crate) seq: Arc<AtomicU64>,
    /// Room ids in model order (`ThumbGuard::Ids` and click routing).
    pub(crate) ids: Arc<Mutex<Vec<String>>>,
    /// Covers already handed to a thumbnail request, so a stale one is dropped.
    pub(crate) covers: Arc<Mutex<Vec<String>>>,
}

/// Log one published row as the UI sees it (`BILI_COMMENT_TOGGLE_TEST`): the
/// model row, its reply count, the chip text and Rust's `expanded` flag. `label`
/// is `open` / `closed`, so the log shows the chip state AFTER the republish.
pub(crate) fn log_published_comment_row(weak: &Weak<PlayerWindow>, row: i32, label: &str) {
    let weak = weak.clone();
    let label = label.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(player) = weak.upgrade() else {
            return;
        };
        let model = player.get_comment_items();
        let item = usize::try_from(row).ok().and_then(|index| model.row_data(index));
        match item {
            Some(item) => {
                let chip = if item.expanded {
                    "收起回复".to_string()
                } else {
                    item.replies_text.to_string()
                };
                crate::debug_log(&format!(
                    "comment-toggle-test {label} row={row} replies={} total={} expanded={} chip={chip}",
                    item.replies.row_count(),
                    item.replies_text,
                    u8::from(item.expanded),
                ))
            }
            None => crate::debug_log(&format!(
                "comment-toggle-test {label} row={row} model_rows={} missing",
                model.row_count()
            )),
        }
    });
}

/// 预热队列里的一个 tab。顺序就是队列顺序。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WarmTab {
    Fav,
    History,
    ToView,
    Region,
    Pgc,
    Live,
    Dynamic,
    Messages,
}

impl WarmTab {
    pub(crate) fn label(self) -> &'static str {
        match self {
            WarmTab::Fav => "fav",
            WarmTab::History => "history",
            WarmTab::ToView => "toview",
            WarmTab::Region => "region",
            WarmTab::Pgc => "pgc",
            WarmTab::Live => "live",
            WarmTab::Dynamic => "dynamic",
            WarmTab::Messages => "messages",
        }
    }

    /// 设置里关掉的 tab 不进队列（目前只有直播有开关）。
    pub(crate) fn enabled(self, prefs: &settings::Settings) -> bool {
        match self {
            WarmTab::Live => !prefs.hide_live_tab,
            _ => true,
        }
    }
}

/// 一个 tab 的预热结果；`items` 是这一页的行数，`covers` 是新拉到的封面数。
pub(crate) struct WarmReport {
    pub(crate) tab: WarmTab,
    pub(crate) ms: u128,
    pub(crate) items: usize,
    pub(crate) covers: usize,
    pub(crate) cached: usize,
    pub(crate) bytes: u64,
    pub(crate) ok: bool,
}

/// Snapshot the single session so a background worker can own it.
pub(crate) fn active_session(session_slot: &SessionSlot) -> Option<Arc<bilibili::Session>> {
    session_slot
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned())
}

#[derive(Clone, Copy)]
pub(crate) enum VipAction {
    Experience,
    Receive,
    Sign,
}

pub(crate) struct DetailState {
    pub(crate) bvid: String,
    pub(crate) title: String,
    pub(crate) cid: u64,
    pub(crate) aid: i64,
    pub(crate) parts: Vec<u64>,
}

#[derive(Default)]
pub(crate) struct FolderState {
    pub(crate) media_id: i64,
    pub(crate) title: String,
    pub(crate) page: i64,
    pub(crate) has_more: bool,
    /// Owner mids of the folder page's rows, aligned with `folder_bvids`.
    pub(crate) mids: Vec<i64>,
}

#[derive(Default)]
pub(crate) struct HistoryPageState {
    pub(crate) max: i64,
    pub(crate) view_at: i64,
    pub(crate) business: String,
    pub(crate) has_more: bool,
    /// Owner mids of the loaded history rows, aligned with `history_bvids`
    /// (append loads extend both in step).
    pub(crate) mids: Vec<i64>,
}

/// The UP page on screen: whose 投稿 it is, its header, and which order was
/// last published.
#[derive(Default)]
pub(crate) struct UpPageState {
    pub(crate) mid: i64,
    pub(crate) name: String,
    /// The header avatar URL from `user_card`, fetched into `up_avatar`.
    pub(crate) avatar: String,
    pub(crate) fans: u64,
    /// The 投稿 count from `user_card`.
    pub(crate) videos: u64,
    /// `acc/info`'s level and signature (the hero's Lv chip and the quiet
    /// two-line signature).
    pub(crate) level: String,
    pub(crate) sign: String,
    /// The active account's relation: 0 unfollowed / 2 following / 6 mutual.
    pub(crate) attribute: i64,
    /// The sort of the page last PUBLISHED (not merely requested): 0 = 播放最多,
    /// 1 = 最新发布. `None` while a load is in flight; the `BILI_UP_PAGE_TEST`
    /// hook polls it.
    pub(crate) published: Option<i32>,
}

#[derive(Default)]
pub(crate) struct RegionState {
    pub(crate) rid: i64,
    pub(crate) name: String,
    pub(crate) page: i64,
    pub(crate) has_more: bool,
}

/// The 番剧 page's backing state. Everything except the published Slint models
/// lives here (models are UI-thread values and are rebuilt from these stores).
#[derive(Default)]
pub(crate) struct PgcState {
    /// 0 = 每日放送, 1..=6 = [`PGC_SEASON_TYPES`] (番剧/国创/电影/纪录片/电视剧/综艺).
    pub(crate) tab: i32,
    /// 0 = 推荐 (分区索引), 1 = 排行榜.
    pub(crate) sub: i32,
    /// 1-based API page of the index list.
    pub(crate) page: i64,
    pub(crate) total: i64,
    pub(crate) has_more: bool,
    pub(crate) loading: bool,
    /// The selected day of the 7-day timeline (index into `timeline`).
    pub(crate) day: i32,
    /// The 分区索引 sort key currently fetched for [`Self::tab`]
    /// ([`PGC_SORTS`] key; empty = 追番最多).
    pub(crate) sort: String,
    pub(crate) timeline: Vec<bilibili::PgcTimelineDay>,
    pub(crate) index_items: Vec<bilibili::PgcIndexItem>,
    pub(crate) rank_items: Vec<bilibili::PgcRankItem>,
}

/// Handles shared by the 番剧 page's callbacks and its loaders. One value, so a
/// handler does not have to capture five separate Arcs.
#[derive(Clone)]
pub(crate) struct PgcHost {
    pub(crate) state: Arc<Mutex<PgcState>>,
    /// Supersedes a slow load (tab switch / fresh page) so a stale response can
    /// never land on a newer page.
    pub(crate) seq: Arc<AtomicU64>,
    /// Season ids of the rows currently published in `pgc_items`.
    pub(crate) covers: Arc<Mutex<Vec<String>>>,
    /// Season ids of the rows currently published in `pgc_eps`.
    pub(crate) ep_covers: Arc<Mutex<Vec<String>>>,
    /// The live preferences: the per-season_type 排序 choice lives in
    /// `settings::Settings::pgc_sorts`, so reading it here keeps the page and
    /// `settings.json` from ever disagreeing.
    pub(crate) settings: Arc<Mutex<settings::Settings>>,
}

/// Main-window handle of the 番剧 page, for the player-side 继续观看 write.
#[derive(Clone)]
pub(crate) struct PgcPageHost {
    pub(crate) ui: Weak<AppWindow>,
    pub(crate) host: PgcHost,
}

#[derive(Default)]
pub(crate) struct SpaceState {
    pub(crate) mid: i64,
    pub(crate) page: i64,
    pub(crate) has_more: bool,
    pub(crate) attribute: i64,
}

#[derive(Default)]
pub(crate) struct RelationState {
    pub(crate) kind: i64,
    pub(crate) page: i64,
    pub(crate) has_more: bool,
}

pub(crate) struct DynamicRowData {
    pub(crate) id: String,
    pub(crate) author: String,
    pub(crate) face: Option<(Vec<u8>, u32, u32)>,
    pub(crate) time: String,
    pub(crate) text: String,
    pub(crate) has_cover: bool,
    pub(crate) cover: Option<(Vec<u8>, u32, u32)>,
    pub(crate) title: String,
    pub(crate) meta: String,
    pub(crate) liked: bool,
    pub(crate) like_text: String,
    pub(crate) stats: String,
}

pub(crate) struct MsgRowData {
    pub(crate) nickname: String,
    pub(crate) avatar: Option<(Vec<u8>, u32, u32)>,
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) time: String,
    pub(crate) count: String,
}

pub(crate) struct SessionRowData {
    pub(crate) talker_id: String,
    pub(crate) name: String,
    pub(crate) avatar: Option<(Vec<u8>, u32, u32)>,
    pub(crate) preview: String,
    pub(crate) time: String,
    pub(crate) unread: String,
}

pub(crate) struct RelationRowData {
    pub(crate) mid: String,
    pub(crate) uname: String,
    pub(crate) face: Option<(Vec<u8>, u32, u32)>,
    pub(crate) sign: String,
    pub(crate) official: String,
}

pub(crate) struct CardRowData {
    pub(crate) pixels: Option<(Vec<u8>, u32, u32)>,
    pub(crate) title: String,
    pub(crate) meta: String,
    pub(crate) badge: String,
}

pub(crate) struct FolderRowData {
    pub(crate) pixels: Option<(Vec<u8>, u32, u32)>,
    pub(crate) title: String,
    pub(crate) meta: String,
    pub(crate) badge: String,
}

pub(crate) struct HistoryRowData {
    pub(crate) pixels: Option<(Vec<u8>, u32, u32)>,
    pub(crate) title: String,
    pub(crate) meta: String,
    pub(crate) badge: String,
}

pub(crate) struct ToViewRowData {
    pub(crate) pixels: Option<(Vec<u8>, u32, u32)>,
    pub(crate) title: String,
    pub(crate) meta: String,
    pub(crate) badge: String,
}

/// Per-list bookkeeping for the streaming cover patches. Shared between the
/// worker stream and the UI patch closures; the summary log line it emits is
/// the proof that every requested cover landed on its own row.
pub(crate) struct ThumbSync {
    pub(crate) label: String,
    pub(crate) total: u64,
    pub(crate) settled: AtomicU64,
    pub(crate) applied: AtomicU64,
    pub(crate) discarded: AtomicU64,
}

impl ThumbSync {
    pub(crate) fn new(label: String, total: usize) -> Arc<Self> {
        Arc::new(Self {
            label,
            total: total as u64,
            settled: AtomicU64::new(0),
            applied: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
        })
    }

    /// UI thread: one requested cover was applied or superseded. When the last
    /// one settles, log `thumbs patched n=<applied>/<total> discarded=<n>`.
    pub(crate) fn settle(&self, applied: bool) {
        if applied {
            self.applied.fetch_add(1, Ordering::SeqCst);
        } else {
            self.discarded.fetch_add(1, Ordering::SeqCst);
        }
        let settled = self.settled.fetch_add(1, Ordering::SeqCst) + 1;
        if settled == self.total {
            crate::debug_log(&format!(
                "{} thumbs patched n={}/{} discarded={}",
                self.label,
                self.applied.load(Ordering::SeqCst),
                self.total,
                self.discarded.load(Ordering::SeqCst),
            ));
        }
    }
}

/// Identity check evaluated on the UI thread just before a cover patch lands.
pub(crate) enum ThumbGuard {
    /// The list is owned by a monotonic sequence counter (search).
    Seq(Arc<AtomicU64>, u64),
    /// The row at `index` must still carry this bvid / dynamic id.
    Ids(Arc<Mutex<Vec<String>>>, String),
    /// The home feed: the generation counter must be current and the row must
    /// still hold the same video.
    GenFeed(Arc<AtomicU64>, u64, Arc<Mutex<Vec<bilibili::FeedVideo>>>, String),
    /// 收藏推荐: the view's generation must be current and the row must still
    /// hold the same video (a tab switch bumps the generation).
    Fav(Arc<AtomicU64>, u64, Arc<Mutex<Vec<bilibili::FeedVideo>>>, String),
}

impl ThumbGuard {
    pub(crate) fn allows(&self, index: usize) -> bool {
        match self {
            ThumbGuard::Seq(seq, my) => seq.load(Ordering::SeqCst) == *my,
            ThumbGuard::Ids(ids, id) => ids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index).cloned())
                .map(|current| current == *id)
                .unwrap_or(false),
            ThumbGuard::GenFeed(generation, my, store, id)
            | ThumbGuard::Fav(generation, my, store, id) => {
                generation.load(Ordering::SeqCst) == *my
                    && store
                        .lock()
                        .ok()
                        .and_then(|guard| guard.get(index).map(|video| video.bvid.clone()))
                        .map(|current| current == *id)
                        .unwrap_or(false)
            }
        }
    }
}

/// One decoded cover: raw RGBA8 pixels plus the dimensions `to_thumb` needs.
pub(crate) type CoverPixels = (Vec<u8>, u32, u32);

/// Bounded, least-recently-used cache of decoded covers, keyed by the image URL
/// exactly as the feed hands it out. Both the streaming decoder and the ring
/// warm-up go through it, so a promoted batch paints what its warm-up already
/// downloaded and decoded instead of fetching the same bytes again.
pub(crate) struct CoverCache {
    pub(crate) entries: HashMap<String, Arc<CoverPixels>>,
    /// Recency order, front = least recently used.
    pub(crate) order: VecDeque<String>,
    pub(crate) bytes: usize,
}

impl CoverCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    pub(crate) fn get(&mut self, url: &str) -> Option<Arc<CoverPixels>> {
        let entry = self.entries.get(url)?.clone();
        self.touch(url);
        Some(entry)
    }

    pub(crate) fn touch(&mut self, url: &str) {
        if let Some(pos) = self.order.iter().position(|key| key == url) {
            self.order.remove(pos);
        }
        self.order.push_back(url.to_string());
    }

    pub(crate) fn put(&mut self, url: &str, pixels: Arc<CoverPixels>) {
        let bytes = pixels.0.len();
        // Empty or oversized decodes are not worth a cache slot; the caller
        // still paints them.
        if bytes == 0 || bytes > COVER_CACHE_BYTES {
            return;
        }
        if let Some(old) = self.entries.remove(url) {
            self.bytes = self.bytes.saturating_sub(old.0.len());
            if let Some(pos) = self.order.iter().position(|key| key == url) {
                self.order.remove(pos);
            }
        }
        self.bytes += bytes;
        self.entries.insert(url.to_string(), pixels);
        self.order.push_back(url.to_string());
        while self.bytes > COVER_CACHE_BYTES || self.entries.len() > COVER_CACHE_ENTRIES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.0.len());
            }
        }
    }
}

/// Which path issued a cover fetch, for the QA counters and nothing else.
#[derive(Clone, Copy)]
pub(crate) enum CoverPath {
    Display,
    Warm,
}

/// Holds the in-flight cover counter for as long as it lives.
pub(crate) struct CoverDecodeGuard;

impl CoverDecodeGuard {
    pub(crate) fn new() -> Self {
        crate::player::note_cover_decode(true);
        Self
    }
}

impl Drop for CoverDecodeGuard {
    fn drop(&mut self) {
        crate::player::note_cover_decode(false);
    }
}

pub(crate) struct SkeletonState {
    pub(crate) timer: slint::Timer,
    pub(crate) remaining: u64,
    pub(crate) phase: f32,
    pub(crate) armed_at: Instant,
}

/// The covers a ring batch carries into its promotion: the first screenful,
/// already decoded, so the first paint of a refreshed page is complete.
pub(crate) struct WarmCovers {
    /// Row index → decoded cover, ascending. Only a prefix of the batch is
    /// here; the rows past it still stream in after promotion.
    pub(crate) rows: Vec<(usize, Arc<CoverPixels>)>,
    /// Covers a full batch would stream (its `THUMB_MAX_PER_VIEW` cap), for the
    /// logs.
    pub(crate) cap: usize,
    /// Rows the first screenful spans ([`feed_cover_warm_target`]), whether or
    /// not every one of them landed.
    pub(crate) target: usize,
}

impl WarmCovers {
    pub(crate) fn new(cap: usize, target: usize) -> Self {
        Self {
            rows: Vec::new(),
            cap,
            target,
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.rows.len()
    }
}

/// One ready-made recommendation batch: exactly what a refresh would fetch,
/// waiting for a promotion.
pub(crate) struct PrefetchedFeedBatch {
    /// Feed source this batch belongs to ([`FEED_SOURCE_RECOMMEND`] for now).
    pub(crate) source: i32,
    /// `fresh_idx` cursor the batch was fetched from (the same value a refresh
    /// passes to `homepage_feed_from`).
    pub(crate) start: u64,
    pub(crate) items: Vec<bilibili::FeedVideo>,
    /// The first screenful's decoded covers, attached at publish time.
    pub(crate) covers: WarmCovers,
    /// When the fetch **and** its cover warm-up finished; drives the ~10
    /// minute memory bound.
    pub(crate) ready_at: Instant,
}

/// The preload ring: ready-made batches waiting to be promoted on refresh.
///
/// Only the bookkeeping lives behind the mutex — a fill never holds the lock
/// across the network, so the UI thread can promote while a refill is in
/// flight. `next_start` is this ring's own cursor and advances by
/// [`bilibili::feed_batch`] per filled slot, so two promotions can never hand
/// out the same batch. `epoch` invalidates a fetch that started before a
/// rebase (the app showed that cursor from the network instead).
pub(crate) struct FeedPrefetchRing {
    pub(crate) source: i32,
    pub(crate) ready: VecDeque<PrefetchedFeedBatch>,
    /// Cursor of the next batch to fill; `0` = not initialised.
    pub(crate) next_start: u64,
    /// Cursor of the fill currently on the network, if any (rebase check).
    pub(crate) in_flight: Option<u64>,
    /// Bumped when an in-flight fill must be discarded.
    pub(crate) epoch: u64,
}

impl FeedPrefetchRing {
    pub(crate) fn new(source: i32) -> Self {
        Self {
            source,
            ready: VecDeque::new(),
            next_start: 0,
            in_flight: None,
            epoch: 0,
        }
    }

    /// Cursor for the next fill: the startup load's cursor until one advances it.
    pub(crate) fn cursor(&self) -> u64 {
        if self.next_start == 0 {
            FEED_START_CURSOR
        } else {
            self.next_start
        }
    }

    /// Drop every batch older than [`FEED_PREFETCH_TTL`]; returns how many.
    pub(crate) fn prune(&mut self) -> usize {
        let before = self.ready.len();
        self.ready
            .retain(|batch| batch.ready_at.elapsed() < FEED_PREFETCH_TTL);
        before - self.ready.len()
    }

    /// Switch the ring to another source: nothing queued for the old source may
    /// ever be shown, so it is dropped and the cursor starts over.
    pub(crate) fn switch_source(&mut self, source: i32) {
        if self.source == source {
            return;
        }
        let dropped = self.ready.len();
        self.ready.clear();
        self.next_start = 0;
        self.in_flight = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.source = source;
        crate::debug_log(&format!(
            "feed prefetch drop source={source} batches={dropped} reason=source"
        ));
    }

    /// Take the oldest ready batch of `source`. Consuming it is what makes a
    /// promotion one-shot: the filler then refills the freed slot.
    pub(crate) fn promote(&mut self, source: i32) -> Option<PrefetchedFeedBatch> {
        self.switch_source(source);
        self.prune();
        while let Some(batch) = self.ready.pop_front() {
            // Defensive: a batch tagged for another source is never shown.
            if batch.source != source {
                continue;
            }
            // Never hand out a cursor that was already promoted, even if a
            // rebase left older entries behind.
            self.ready.retain(|queued| queued.start > batch.start);
            return Some(batch);
        }
        None
    }

    /// The app fetched the batch at `start` on the network path (startup
    /// fallback, 换一换, a page turn), so a queued or in-flight copy of it would
    /// be the same content twice: drop/rebase past it.
    pub(crate) fn rebase_past(&mut self, start: u64) {
        let before = self.ready.len();
        self.ready.retain(|batch| batch.start > start);
        if self.in_flight.map(|cursor| cursor <= start).unwrap_or(false) {
            self.epoch = self.epoch.wrapping_add(1);
        }
        let next = start.saturating_add(bilibili::feed_batch().max(1));
        self.next_start = self.cursor().max(next);
        crate::debug_log(&format!(
            "feed prefetch rebase start={start} next_start={} dropped={}",
            self.next_start,
            before - self.ready.len()
        ));
    }
}

/// True once the home view has been revealed after login. Written on the UI
/// thread and read by the prefetch QA hook's worker (an atomic, because reading
/// Slint properties off the UI thread is not allowed).
pub(crate) static HOME_SHOWN: AtomicBool = AtomicBool::new(false);

/// One cover a rendered list still has to fetch: the row it belongs to, its CDN
/// url and its bvid (the patch guard's identity), in the order they stream.
pub(crate) struct PendingCover {
    pub(crate) index: usize,
    pub(crate) pic: String,
    pub(crate) bvid: String,
}

/// UI thread: show one feed batch's text rows and point the pager at
/// `target_page`. No network happens here — the covers come from
/// [`stream_feed_covers`] on a worker. Returns whether the batch was published
/// (a superseded generation is dropped instead).
pub(crate) fn publish_feed_rows(
    ui: &AppWindow,
    generation: &Arc<AtomicU64>,
    my: u64,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    videos: Vec<bilibili::FeedVideo>,
    target_page: i64,
    cap: usize,
) -> bool {
    publish_feed_rows_with_covers(
        ui,
        generation,
        my,
        feed_store,
        videos,
        target_page,
        cap,
        &[],
    )
}

/// [`publish_feed_rows`] with a ring batch's already-decoded covers attached to
/// their rows before the model goes live: a promoted batch's first paint is
/// complete, so nothing streams into the first screenful afterwards.
pub(crate) fn publish_feed_rows_with_covers(
    ui: &AppWindow,
    generation: &Arc<AtomicU64>,
    my: u64,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    videos: Vec<bilibili::FeedVideo>,
    target_page: i64,
    cap: usize,
    covers: &[(usize, Arc<CoverPixels>)],
) -> bool {
    if generation.load(Ordering::SeqCst) != my {
        return false;
    }
    let mut items: Vec<FeedRow> = videos.iter().map(build_feed_row_text).collect();
    for (index, pixels) in covers {
        if let Some(row) = items.get_mut(*index) {
            row.thumb = to_thumb_shared(pixels);
        }
    }
    if let Ok(mut guard) = feed_store.lock() {
        *guard = videos;
    }
    ui.set_feed_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    publish_refresh(ui, false, 1.0);
    // The bar's counter is the client page over this batch; a new batch
    // lands on page 0, a refresh keeps the page it reloaded.
    set_client_page(0, target_page);
    publish_list_offset(ui);
    // Only covers that still have to arrive arm the shimmer: a warm batch's
    // attached rows are already complete.
    skeleton_arm(ui, cap.saturating_sub(covers.len()) as u64);
    skeleton_settle(ui, 1);
    update_page_bar(ui);
    if std::env::var("BILI_AUTOPLAY")
        .map(|value| value == "1")
        .unwrap_or(false)
    {
        ui.invoke_open_video(0);
    }
    // Diagnostic hook (mirrors BILI_AUTOPLAY): open a SECOND video a few
    // seconds after the first, reproducing the multi-player case from the
    // backend. `BILI_MULTI_TEST=<delay_ms>` (default 6000).
    if let Ok(value) = std::env::var("BILI_MULTI_TEST") {
        let delay = value.trim().parse::<u64>().unwrap_or(6000);
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    crate::debug_log(&format!("multi-test open second video delay={delay}"));
                    ui.invoke_open_video(1);
                }
            });
        });
    }

    // Multi-open QA (`BILI_MULTI_OPEN_TEST=<gap_ms>`, default 7000): opens two
    // real player windows through card_open mode 1 (the same path the UI's right
    // click uses), so the per-window pointer watch can be proven with the probe.
    if let Ok(value) = std::env::var("BILI_MULTI_OPEN_TEST") {
        let gap = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(7000)
            .clamp(1000, 60_000);
        let weak = ui.as_weak();
        let feed_store = feed_store.clone();
        std::thread::spawn(move || {
            for _ in 0..240 {
                if feed_store
                    .lock()
                    .map(|feed| !feed.is_empty())
                    .unwrap_or(false)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            for index in [0, 1] {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        crate::debug_log(&format!(
                            "multi-open-test invoke card_open index={index} mode=1"
                        ));
                        ui.invoke_card_open(index, 1);
                    }
                });
                std::thread::sleep(Duration::from_millis(gap));
            }
            crate::debug_log("multi-open-test done");
        });
    }

    // Card-mode QA (`BILI_CARD_TEST=<gap_ms>`, default 4000): drives the same
    // three modes the UI's card_open callback delivers. The window/queue effects
    // are logged by the callbacks themselves (`card open`, `player session
    // open`, `player close`, `card enqueue`). No synthetic input.
    if let Ok(value) = std::env::var("BILI_CARD_TEST") {
        let gap = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(4000)
            .clamp(1000, 30_000);
        let weak = ui.as_weak();
        let feed_store = feed_store.clone();
        std::thread::spawn(move || {
            for _ in 0..240 {
                if feed_store
                    .lock()
                    .map(|feed| !feed.is_empty())
                    .unwrap_or(false)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            for (index, mode, label) in [
                (0, 1, "new-window-1"),
                (1, 1, "new-window-2"),
                (2, 2, "enqueue"),
                (3, 0, "replace"),
            ] {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        crate::debug_log(&format!(
                            "card-test invoke index={index} mode={mode} step={label}"
                        ));
                        ui.invoke_card_open(index, mode);
                    }
                });
                std::thread::sleep(Duration::from_millis(gap));
            }
            crate::debug_log("card-test done");
        });
    }

    // Window-move probe (`BILI_WINDOW_PROBE_TEST=<gap_ms>`, default 1500) is
    // started by `open_player_inner` once two windows exist: it moves only the
    // WINDOW rects (`SetWindowPos`) under a stationary cursor and runs on the UI
    // thread (a Slint timer), because it needs `Rc<PlayerState>`.

    // Diagnostic hook (mirrors BILI_AUTOPLAY): open the detail view for one
    // feed row once the feed has loaded, so the detail view can be captured
    // from PrintWindow / BILI_DEBUG_LOG without synthetic input.
    if let Ok(value) = std::env::var("BILI_DETAIL_ON_START") {
        if let Ok(index) = value.trim().parse::<i32>() {
            crate::debug_log(&format!("detail-test open index={index}"));
            ui.invoke_open_feed_detail(index);
        }
    }
    // Diagnostic hook (mirrors BILI_DETAIL_ON_START): open the settings
    // panel after the feed has loaded so its 账户 section (会员中心 /
    // 退出登录) can be captured from PrintWindow without synthetic input.
    if std::env::var("BILI_SETTINGS_ON_START")
        .map(|value| value == "1")
        .unwrap_or(false)
    {
        crate::debug_log("settings-test open");
        ui.invoke_open_settings();
    }
    // Diagnostic hook: exercise the whole playback queue through its real
    // callbacks once the feed it enqueues from is on screen.
    maybe_start_playlist_test(ui);
    true
}

/// Phase B: stream one feed batch's remaining covers into the rows that
/// [`publish_feed_rows`] already rendered. `pending` is ordered by row, so a
/// warm batch skips exactly the covers its promotion already attached. Every
/// patch is guarded by the generation and the row's bvid, so a superseded batch
/// can never paint over a newer one. `first_screen` is the row count the ring's
/// warm-up covers; any fetch issued below it is a warm-up miss (diagnostic).
pub(crate) async fn stream_feed_covers(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session: &Arc<bilibili::Session>,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    pending: Vec<PendingCover>,
    first_screen: usize,
    label: &str,
) {
    let thumbs_started = Instant::now();
    let fallbacks_before = bilibili::thumb_fallbacks();
    let total = pending.len();
    let sync = ThumbSync::new(label.to_string(), total);
    let weak_b = weak.clone();
    let generation_b = generation.clone();
    let feed_store_b = feed_store.clone();
    let indices: Vec<usize> = pending.iter().map(|cover| cover.index).collect();
    let bvids: Vec<String> = pending.iter().map(|cover| cover.bvid.clone()).collect();
    let pics: Vec<String> = pending.iter().map(|cover| cover.pic.clone()).collect();
    let indices_issue = indices.clone();
    let count_first_screen = move |slot: usize| {
        if indices_issue.get(slot).copied().unwrap_or(usize::MAX) < first_screen {
            COVER_FETCHES_FIRST_SCREEN.fetch_add(1, Ordering::Relaxed);
        }
    };
    let mut delivered = 0usize;
    stream_thumbs(session, pics, count_first_screen, |slot, pixels| {
        delivered += 1;
        let index = indices.get(slot).copied().unwrap_or(slot);
        let bvid = bvids.get(slot).cloned().unwrap_or_default();
        let guard = ThumbGuard::GenFeed(generation_b.clone(), my, feed_store_b.clone(), bvid);
        let weak_c = weak_b.clone();
        let sync_c = sync.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak_c.upgrade() {
                let applied = guard.allows(index);
                if applied {
                    patch_model_thumb(ui.get_feed_items(), index, to_thumb(&pixels), |row, image| {
                        row.thumb = image
                    });
                }
                skeleton_settle(&ui, 1);
                sync_c.settle(applied);
            }
        });
    })
    .await;
    crate::debug_log(&format!(
        "feed thumbs n={} fallback={} ms={} label={label}",
        delivered,
        bilibili::thumb_fallbacks().saturating_sub(fallbacks_before),
        thumbs_started.elapsed().as_millis()
    ));
    flush_missing(weak, &sync, total.saturating_sub(delivered));
}

/// Outcome of waiting for the ring's first batch on the startup path.
pub(crate) enum StartupBatch {
    Ready(PrefetchedFeedBatch),
    Timeout,
    /// A newer login attempt superseded this one; leave the UI alone.
    Superseded,
}

/// Debounced (140 ms) suggestion lookup; a newer keystroke makes this inert.
pub(crate) fn load_suggestions(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    suggest_seq: Arc<AtomicU64>,
    text: String,
) {
    let seq = suggest_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let keyword = text.trim().to_string();
    if keyword.is_empty() {
        if let Some(ui) = weak.upgrade() {
            ui.set_suggest_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                Vec::<SharedString>::new(),
            ))));
        }
        return;
    }
    // A new keystroke invalidates the previous list immediately: until the
    // debounced reply lands the panel must keep showing the hot list (never a
    // stale suggestion list or an empty gap). `suggest_seq` already makes the
    // in-flight request inert.
    if let Some(ui) = weak.upgrade() {
        ui.set_suggest_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<SharedString>::new(),
        ))));
    }
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            tokio::time::sleep(Duration::from_millis(140)).await;
            if suggest_seq.load(Ordering::SeqCst) != seq {
                return;
            }
            let items = session.search_suggest(&keyword).await.unwrap_or_default();
            if suggest_seq.load(Ordering::SeqCst) != seq {
                return;
            }
            let _ = slint::invoke_from_event_loop(move || {
                if suggest_seq.load(Ordering::SeqCst) != seq {
                    return;
                }
                if let Some(ui) = weak.upgrade() {
                    let list: Vec<SharedString> =
                        items.into_iter().map(SharedString::from).collect();
                    ui.set_suggest_items(ModelRc::from(std::rc::Rc::new(VecModel::from(list))));
                }
            });
        });
    }));
}

/// Fetch the 热搜 list (display names + search keywords) for the results view.
pub(crate) fn load_hot_search(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    hot_keywords: Arc<Mutex<Vec<String>>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let list = session.hot_search().await.unwrap_or_default();
            let display: Vec<SharedString> = list
                .iter()
                .map(|hot| SharedString::from(hot.show_name.clone()))
                .collect();
            let keywords: Vec<String> = list.into_iter().map(|hot| hot.keyword).collect();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut guard) = hot_keywords.lock() {
                        *guard = keywords;
                    }
                    ui.set_hot_items(ModelRc::from(std::rc::Rc::new(VecModel::from(display))));
                }
            });
        });
    }));
}

/// Which of the two folder-scoped 收藏 tabs a load serves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FavMode {
    /// 推荐流: the 收藏推荐 feed (mix / folder as a seed).
    Stream,
    /// 收藏夹: the plain listing (all folders / one folder, no expansion).
    Library,
}

impl FavMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            FavMode::Stream => "stream",
            FavMode::Library => "library",
        }
    }
}

/// UI-side truth for the 收藏 page. The generation is bumped by every load
/// request, so a worker that finishes after a tab switch is dropped.
pub(crate) struct FavViewState {
    /// Published order: `fav_tabs[i]` is `folders[i].name`, tab `i + 1`.
    pub(crate) folders: Vec<bilibili::FavFolder>,
    /// Tab the last load asked for (`0` = 全部, `i + 1` = `folders[i]`).
    pub(crate) tab: i32,
    /// Tab (mode + folder) the last load asked for.
    pub(crate) mode: FavMode,
    /// The tab whose cards are on screen; `None` while a load is in flight.
    pub(crate) published: Option<(FavMode, i32)>,
    /// A load is in flight (the QA hook polls this).
    pub(crate) loading: bool,
    /// The folder fetch is in flight / has completed at least once.
    pub(crate) folders_loading: bool,
    pub(crate) folders_loaded: bool,
    pub(crate) generation: Arc<AtomicU64>,
    /// The videos behind the current `masonry`, for `masonry_open`.
    pub(crate) videos: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
}

impl FavViewState {
    pub(crate) fn new() -> Self {
        Self {
            folders: Vec::new(),
            tab: 0,
            mode: FavMode::Stream,
            published: None,
            loading: false,
            folders_loading: false,
            folders_loaded: false,
            generation: Arc::new(AtomicU64::new(0)),
            videos: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// One built tab, with the count of folders that contributed to a mix.
#[derive(Clone)]
pub(crate) struct FavCacheEntry {
    pub(crate) at: Instant,
    pub(crate) cards: Vec<bilibili::FeedVideo>,
    pub(crate) sources: usize,
}

/// One mode's cache: the 全部 entry (keyed by the folder set) and the per-folder
/// entries. 推荐流 and 收藏夹 build different cards from the same folders, so
/// they keep separate namespaces.
#[derive(Default)]
pub(crate) struct FavCacheNs {
    /// Folder ids the 全部 entry was built from: a different set invalidates it.
    pub(crate) key: Vec<i64>,
    pub(crate) all: Option<FavCacheEntry>,
    pub(crate) folders: HashMap<i64, FavCacheEntry>,
}

#[derive(Default)]
pub(crate) struct FavViewCache {
    pub(crate) stream: FavCacheNs,
    pub(crate) library: FavCacheNs,
}

/// One card's cover still to arrive.
pub(crate) struct FavPendingCover {
    pub(crate) index: usize,
    pub(crate) pic: String,
    pub(crate) bvid: String,
}

/// Replace one masonry row's cover in place (never re-publishes the list).
pub(crate) fn patch_masonry_row(model: ModelRc<MasonryCard>, index: usize, image: Image, aspect: f32) {
    if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<MasonryCard>>() {
        if let Some(mut row) = vec_model.row_data(index) {
            row.cover = image;
            row.aspect = aspect;
            vec_model.set_row_data(index, row);
        }
    }
}

/// Run one search query on a worker thread and push the rows to the UI.
pub(crate) fn run_search(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_seq: Arc<AtomicU64>,
    search_bvids: Arc<Mutex<Vec<String>>>,
) {
    let (keyword, tab, page) = match search_state.lock() {
        Ok(state) => (state.keyword.clone(), state.tab, state.page),
        Err(_) => return,
    };
    if keyword.trim().is_empty() {
        return;
    }
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    // One API page per screen: fetch with the live client page size so the
    // loaded page fills exactly one screen.
    let ps = current_page_size(&weak);
    let seq = search_seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        update_page_bar(&ui);
    }

    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let outcome = if tab == 0 {
                session
                    .search_video(&keyword, page, "totalrank", ps)
                    .await
                    .map(|result| SearchOutcome {
                        page: result.page,
                        num_pages: result.num_pages,
                        num_results: result.num_results,
                        rows: result
                            .items
                            .iter()
                            .map(|item| {
                                let meta = format!(
                                    "{} · {}播放 · {}",
                                    item.author,
                                    format_view(item.play),
                                    item.duration
                                );
                                (
                                    item.pic.clone(),
                                    SearchRowData {
                                        pixels: None,
                                        title: item.title.clone(),
                                        meta,
                                        sub: item.description.clone(),
                                        badge: item.duration.clone(),
                                        owner: item.author.clone(),
                                        owner_mid: item.mid,
                                        view_count: item.play,
                                        pub_ts: item.pub_ts,
                                    },
                                )
                            })
                            .collect(),
                        bvids: result.items.iter().map(|item| item.bvid.clone()).collect(),
                        mids: result.items.iter().map(|item| item.mid).collect(),
                    })
                    .map_err(|err| format!("{err}"))
            } else {
                session
                    .search_user(&keyword, page, ps)
                    .await
                    .map(|result| SearchOutcome {
                        page: result.page,
                        num_pages: result.num_pages,
                        num_results: result.num_results,
                        rows: result
                            .items
                            .iter()
                            .map(|item| {
                                let meta =
                                    format!("粉丝 {} · 投稿 {}", format_view(item.fans), item.videos);
                                (
                                    item.upic.clone(),
                                    SearchRowData {
                                        pixels: None,
                                        title: item.uname.clone(),
                                        meta,
                                        sub: item.usign.clone(),
                                        badge: String::new(),
                                        owner: String::new(),
                                        owner_mid: item.mid,
                                        view_count: 0,
                                        pub_ts: 0,
                                    },
                                )
                            })
                            .collect(),
                        bvids: Vec::new(),
                        mids: result.items.iter().map(|item| item.mid).collect(),
                    })
                    .map_err(|err| format!("{err}"))
            };

            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(message) => {
                    let state_empty = search_state.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if search_seq.load(Ordering::SeqCst) != seq {
                            return;
                        }
                        if let Ok(mut state) = state_empty.lock() {
                            state.mids.clear();
                        }
                        if let Some(ui) = weak.upgrade() {
                            let empty: Vec<SearchRow> = Vec::new();
                            ui.set_search_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                empty,
                            ))));
                            publish_refresh(&ui, false, 1.0);
                            ui.set_search_status(format!("搜索失败：{message}").into());
                            ui.set_search_page_text(String::new().into());
                            ui.set_search_current_page(1);
                            ui.set_search_has_prev(false);
                            ui.set_search_has_next(false);
                            ui.set_search_total_pages(1);
                            set_client_page(1, 0);
                            publish_list_offset(&ui);
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };

            let pics: Vec<String> = outcome.rows.iter().map(|(url, _)| url.clone()).collect();
            let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<SearchRowData> = outcome.rows.into_iter().map(|(_, row)| row).collect();
            let bvids = outcome.bvids;
            let mids = outcome.mids;
            let page = outcome.page;
            let num_pages = outcome.num_pages;
            let num_results = outcome.num_results;

            let loaded = rows.len();
            let weak_a = weak.clone();
            let search_seq_a = search_seq.clone();
            let search_bvids_a = search_bvids.clone();
            let search_state_a = search_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if search_seq_a.load(Ordering::SeqCst) != seq {
                    return;
                }
                if let Some(ui) = weak_a.upgrade() {
                    if let Ok(mut guard) = search_bvids_a.lock() {
                        *guard = bvids;
                    }
                    if let Ok(mut state) = search_state_a.lock() {
                        state.mids = mids;
                    }
                    let items: Vec<SearchRow> = rows
                        .iter()
                        .map(|row| SearchRow {
                            thumb: to_thumb(&row.pixels),
                            title: row.title.clone().into(),
                            meta: row.meta.clone().into(),
                            sub: row.sub.clone().into(),
                            badge: row.badge.clone().into(),
                            owner: row.owner.clone().into(),
                            owner_mid: row.owner_mid.to_string().into(),
                            view_count: saturating_i32(row.view_count as i64),
                            pub_ts: saturating_i32(row.pub_ts),
                        })
                        .collect();
                    // One API page per screen: the client page restarts at 0
                    // and the bar counts pages of the live page size.
                    let total = search_total(num_results, num_pages, list_page_size(&ui));
                    if let Ok(mut state) = search_state_a.lock() {
                        state.num_pages = num_pages.max(1);
                    }
                    set_client_page(1, 0);
                    ui.set_search_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_search_status(String::new().into());
                    ui.set_search_page_text(
                        format!("第 {}/{} 页", page.max(1), total).into(),
                    );
                    ui.set_search_current_page(page.max(1) as i32);
                    ui.set_search_has_prev(page > 1);
                    ui.set_search_has_next(page < total);
                    ui.set_search_total_pages(total as i32);
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("search load rendered items={loaded}"));
                }
            });
            let mut pics = pics;
            pics.truncate(covers);
            let seq_guard = search_seq.clone();
            stream_cover_list(
                &weak,
                &session,
                pics,
                Vec::new(),
                0,
                move |_index, _key| ThumbGuard::Seq(seq_guard.clone(), seq),
                |ui| ui.get_search_items(),
                |row, image| row.thumb = image,
                format!("search tab={tab} page={page}"),
            )
            .await;
            // One card-health line after the patches have landed: covers are
            // the same shared cache + decode path as the feed, and the home
            // card fields (UP / heat / date) must be populated for every row.
            let weak_cards = weak.clone();
            let seq_cards = search_seq.clone();
            let label = format!("search tab={tab} page={page}");
            let _ = slint::invoke_from_event_loop(move || {
                slint::Timer::single_shot(Duration::from_millis(250), move || {
                    if seq_cards.load(Ordering::SeqCst) != seq {
                        return;
                    }
                    if let Some(ui) = weak_cards.upgrade() {
                        let rows = ui.get_search_items();
                        let total = rows.row_count();
                        let mut covers = 0usize;
                        let mut owners = 0usize;
                        let mut dates = 0usize;
                        for index in 0..total {
                            let Some(row) = rows.row_data(index) else {
                                continue;
                            };
                            covers += usize::from(row.thumb.size().width > 0);
                            owners += usize::from(!row.owner.is_empty());
                            dates += usize::from(row.pub_ts > 0);
                        }
                        crate::debug_log(&format!(
                            "search cards {label} covers={covers}/{total} owners={owners}/{total} dates={dates}/{total}"
                        ));
                    }
                });
            });
        });
    }));
}

/// 养鱼 UI-side state: the engine plus the gates that keep a pushed snapshot
/// from rebuilding Slint models more than twice a second.
pub(crate) struct FishUi {
    pub(crate) engine: Option<Arc<fish::FishEngine>>,
    /// `(bvid, at, sort, filter, cleared_at, local day)` of the two card lists
    /// currently in the models.
    pub(crate) last_verdict: RefCell<Option<(String, i64, i32, i32, i64, i64)>>,
    /// When the stats model was last rebuilt.
    pub(crate) last_stats: RefCell<Option<Instant>>,
    /// The local day the page was last published for: the tick that sees a new
    /// day republishes everything, so 今日 rolls over without a restart.
    pub(crate) day: Cell<u32>,
    /// `FishEngine::open` failed: the page opens in its error state.
    pub(crate) open_error: RefCell<bool>,
    /// `(has_key, masked)` mirror of `prefs.deepseek_key`.
    pub(crate) key_state: RefCell<(bool, String)>,
    /// Cover URLs already attempted (a failed fetch must not retry forever).
    pub(crate) tried_covers: RefCell<HashSet<String>>,
    /// The feed toolbar's current query, owned by Rust because it rebuilds the
    /// model; the UI mirrors it in `fish_sort` / `fish_filter`.
    pub(crate) feed_sort: Cell<i32>,
    pub(crate) feed_filter: Cell<i32>,
    /// bvids opened from the 推荐流 feed: "adopted" for the fish's growth. Every
    /// bvid counts once per session, so re-opening a card does not inflate it.
    pub(crate) adopted: RefCell<HashSet<String>>,
    /// The home feed the app already holds. A verdict written by an older build
    /// can carry `cover = ''`; the feed's own `pic` for that bvid is the first
    /// fallback before the card's tinted glyph block.
    pub(crate) feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    /// The live preferences, so the 看片力度 rows can show tier + switches
    /// without re-reading `settings.json` on every snapshot.
    pub(crate) settings_store: Arc<Mutex<settings::Settings>>,
}

/// The parts of the 鱼缸 configuration the auto-feed timer needs, kept on the UI
/// thread so a 20s tick never reads `settings.json` from disk. `last_day` is the
/// local day-of-year of the last automatic run: one run per day, and turning
/// auto-feed on never fires the same day's already-passed slot.
pub(crate) struct FishAuto {
    pub(crate) enabled: bool,
    pub(crate) auto: bool,
    pub(crate) time_minutes: u32,
    pub(crate) sources: u32,
    pub(crate) last_day: Option<u32>,
}

impl FishUi {
    pub(crate) fn error_text(&self) -> String {
        if *self.open_error.borrow() {
            "fish.db 打不开（检查目录权限）".to_string()
        } else {
            String::new()
        }
    }
}

/// Local wall clock as `(minutes since midnight, day key)`.
///
/// `GetLocalTime` is the only local-date API this binary needs, and a
/// `month * 32 + day` key is unique within a year - enough for "one automatic
/// feed per day" without a calendar dependency.
#[repr(C)]
pub(crate) struct WinLocalTime {
    pub(crate) year: u16,
    pub(crate) month: u16,
    pub(crate) day_of_week: u16,
    pub(crate) day: u16,
    pub(crate) hour: u16,
    pub(crate) minute: u16,
    pub(crate) second: u16,
    pub(crate) milliseconds: u16,
}

/// Alpha-composite `fg` over `bg` (both sRGB bytes); used only for the log.
pub(crate) fn composite_over(fg: slint::Color, bg: slint::Color) -> slint::Color {
    let alpha = fg.alpha() as f32 / 255.0;
    let mix = |front: u8, back: u8| -> u8 {
        (front as f32 * alpha + back as f32 * (1.0 - alpha)).round() as u8
    };
    slint::Color::from_rgb_u8(
        mix(fg.red(), bg.red()),
        mix(fg.green(), bg.green()),
        mix(fg.blue(), bg.blue()),
    )
}

/// WCAG 2.x relative-luminance contrast ratio, for the tagline measurement.
pub(crate) fn contrast_ratio(a: slint::Color, b: slint::Color) -> f32 {
    pub(crate) fn channel(value: u8) -> f32 {
        let s = value as f32 / 255.0;
        if s <= 0.039_28 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    pub(crate) fn luminance(color: slint::Color) -> f32 {
        0.2126 * channel(color.red())
            + 0.7152 * channel(color.green())
            + 0.0722 * channel(color.blue())
    }
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// `1234` → `1.2k`, `940` → `940`. One rule for every token number on the page.
pub(crate) fn fmt_tokens(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// One line of header small print:
/// `最近 7 天：12 条 · 看 7 · 不看 3 · 也许 2 · 8.4k tok`.
pub(crate) fn fmt_window(label: &str, counts: &fish::FishCounts) -> String {
    format!(
        "{label}：{} 条 · 看 {} · 不看 {} · 也许 {} · {} tok",
        counts.watched,
        counts.recommend,
        counts.skip,
        counts.maybe,
        fmt_tokens(counts.tokens())
    )
}

/// Publish the 看片力度 rows. `available` is the honest gate: a step whose
/// backend is missing (engine not installed, not logged in) is shown disabled
/// with the reason, never as a switch that silently does nothing.
pub(crate) fn publish_watch_steps(ui: &AppWindow, fish: &FishUi) {
    let prefs = fish
        .settings_store
        .lock()
        .map(|prefs| prefs.clone())
        .unwrap_or_default();
    let tier = settings::normalise_fish_tier(prefs.fish_tier);
    let selection = if tier < settings::FISH_TIER_CUSTOM {
        fishwatch::StepSelection::for_tier(tier)
    } else {
        fishwatch::StepSelection {
            meta: prefs.fish_step_meta,
            subtitle: prefs.fish_step_subtitle,
            outline: prefs.fish_step_outline,
            comments: prefs.fish_step_comments,
            asr: prefs.fish_step_asr,
            frames: prefs.fish_step_frames,
        }
    };
    let logged_in = ui.get_logged_in();
    let mut config = fish::watch_config(&prefs);
    config.online = logged_in;
    // Never probe the engine here: `publish_watch_steps` runs while the first
    // window is still being built, and a cold `import ctranslate2` would hold
    // the app invisible behind a python console. Read the cache, ask for a
    // background probe, and say "检测中…" until it lands.
    let probe = fishwatch::asr::snapshot(&config);
    if probe.is_none() {
        fishwatch::asr::warm(&config);
    }
    let asr_engine = probe.clone().flatten();
    let asr_model_ready = fishwatch::asr::model_ready(&config);
    let asr_note = if !config.asr.enabled {
        ""
    } else {
        match (&probe, asr_model_ready) {
            (None, _) => "检测中…",
            (Some(None), _) => "引擎未安装",
            (Some(Some(_)), false) => "模型未下载",
            _ => "",
        }
    };
    let frames_note = fishwatch::frames::preflight(&config);
    let mut rows: Vec<WatchStepRow> = Vec::new();
    for id in fishwatch::steps::StepId::ALL {
        let (desc, estimate) = match id {
            fishwatch::steps::StepId::Meta => ("标题 / UP / 时长 / 分区", "0–0.8s"),
            fishwatch::steps::StepId::Subtitle => ("B 站官方字幕，没有就跳过", "0.3–1.5s"),
            fishwatch::steps::StepId::Outline => ("带时间点的 AI 提纲", "0.5–2s"),
            fishwatch::steps::StepId::Comments => ("热评 + 弹幕峰值", "0.5–2s"),
            fishwatch::steps::StepId::Asr => ("下载音频，本机 Whisper 转写", "30–300s"),
            fishwatch::steps::StepId::Frames => ("480p 抽样画面，DeepSeek 视觉", "10–90s"),
        };
        let (available, note) = match id {
            fishwatch::steps::StepId::Meta => (true, ""),
            fishwatch::steps::StepId::Subtitle
            | fishwatch::steps::StepId::Outline
            | fishwatch::steps::StepId::Comments => {
                if logged_in {
                    (true, "")
                } else {
                    (false, "未登录")
                }
            }
            fishwatch::steps::StepId::Asr => {
                (asr_engine.is_some() && asr_model_ready, asr_note)
            }
            fishwatch::steps::StepId::Frames => (
                frames_note.is_none(),
                frames_note.as_deref().unwrap_or(""),
            ),
        };
        rows.push(WatchStepRow {
            id: id.as_str().into(),
            label: id.label().into(),
            desc: desc.into(),
            estimate: estimate.into(),
            on: selection.get(id),
            available,
            note: note.into(),
        });
    }
    ui.set_fish_steps(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
    crate::debug_log(&format!(
        "fish watch tiers tier={tier} selection={:?}",
        selection.ids().iter().map(|id| id.as_str()).collect::<Vec<_>>()
    ));
}

#[cfg(test)]
mod up_resolver_tests {
    use super::*;

    pub(crate) fn store(items: &[(i64, &str)]) -> Arc<Mutex<Vec<bilibili::FeedVideo>>> {
        let videos: Vec<bilibili::FeedVideo> = items
            .iter()
            .map(|(mid, bvid)| bilibili::FeedVideo {
                bvid: (*bvid).to_string(),
                owner_mid: *mid,
                ..bilibili::FeedVideo::default()
            })
            .collect();
        Arc::new(Mutex::new(videos))
    }

    /// The card's UP name must resolve against the view on screen, never against
    /// the home feed: feed rows, the 用户 search tab, a favourite folder page and
    /// the history list each answer from their own list, and the UP overlay's own
    /// page wins while it is open.
    #[test]
    pub(crate) fn card_mid_follows_the_view_on_screen() {
        let feed = store(&[(11, "BVfeed")]);
        let masonry = store(&[(66, "BVmasonry")]);
        let up = store(&[(22, "BVup")]);
        let search = Arc::new(Mutex::new(SearchState {
            tab: 1,
            mids: vec![33, 0],
            ..SearchState::default()
        }));
        let folder = Arc::new(Mutex::new(FolderState {
            mids: vec![44],
            ..FolderState::default()
        }));
        let history = Arc::new(Mutex::new(HistoryPageState {
            mids: vec![0, 55],
            ..HistoryPageState::default()
        }));

        let mid = |view, index, up_open, on_masonry| {
            resolve_card_mid(
                view,
                index,
                up_open,
                on_masonry,
                &up,
                &feed,
                &masonry,
                &search,
                &folder,
                &history,
            )
        };

        // View 0: the feed row carries its owner; the 收藏推荐 tab answers from
        // its own masonry list instead.
        assert_eq!(mid(0, 0, false, false), Some(11));
        assert_eq!(mid(0, 0, false, true), Some(66));
        // The UP overlay's list wins while the page is open.
        assert_eq!(mid(0, 0, true, false), Some(22));
        // 用户 search row, favourite-folder row, second history row.
        assert_eq!(mid(1, 0, false, false), Some(33));
        assert_eq!(mid(4, 0, false, false), Some(44));
        assert_eq!(mid(5, 1, false, false), Some(55));
        // A missing owner, a stale index or a view with no mids never guess.
        assert_eq!(mid(1, 1, false, false), None);
        assert_eq!(mid(5, 9, false, false), None);
        assert_eq!(mid(13, 0, false, false), None);
        assert_eq!(mid(0, -1, false, false), None);
    }
}

#[cfg(test)]
mod chrome_idle_tests {
    use super::*;

    /// One 16 ms tick, like the real pointer watch.
    pub(crate) fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    pub(crate) fn movement_never_hides_the_chrome() {
        let base = Instant::now();
        let mut idle = ChromeIdle::new(base);
        for step in 0u64..400 {
            let cursor = Some((100 + (step % 2) as i32, 100));
            assert!(
                idle.poll(cursor, true, false, at(base, step * 16)),
                "1 px movement at {} ms must keep the chrome up",
                step * 16
            );
        }
        assert!(!idle.hidden);
    }

    #[test]
    pub(crate) fn stillness_hides_after_the_threshold_and_hold_suppresses_it() {
        let base = Instant::now();
        let mut idle = ChromeIdle::new(base);
        assert!(idle.poll(Some((10, 10)), true, false, at(base, 0)));
        // Just below the threshold the chrome is still up.
        assert!(idle.poll(Some((10, 10)), true, false, at(base, 2_400)));
        assert!(!idle.hidden);
        // Past it, the chrome hides and stays hidden.
        assert!(!idle.poll(Some((10, 10)), true, false, at(base, 2_501)));
        assert!(idle.hidden);
        assert!(!idle.poll(Some((10, 10)), true, false, at(base, 4_000)));
        // A held button (or chrome_hold) shows it again immediately.
        assert!(idle.poll(Some((10, 10)), true, true, at(base, 4_100)));
        assert!(!idle.hidden);
        // Releasing with no movement starts a fresh streak.
        assert!(idle.poll(Some((10, 10)), true, false, at(base, 4_200)));
    }

    #[test]
    pub(crate) fn leaving_and_re_entering_resets_the_streak() {
        let base = Instant::now();
        let mut idle = ChromeIdle::new(base);
        assert!(idle.poll(Some((10, 10)), true, false, at(base, 0)));
        assert!(!idle.poll(Some((10, 10)), true, false, at(base, 3_000)));
        assert!(idle.hidden);
        // Left the window: not visible, and the idle latch is cleared.
        assert!(!idle.poll(Some((900, 900)), false, false, at(base, 3_100)));
        assert!(!idle.hidden);
        // Back inside without moving the cursor: entering is activity.
        assert!(idle.poll(Some((900, 900)), true, false, at(base, 3_200)));
        assert!(!idle.hidden);
    }
}

/// The 画质 menu merge: locked 大会员 tiers are rows that resolve to no stream.
#[cfg(test)]
mod quality_menu_tests {
    use super::{build_quality_menu, LockedNotice, QualityMenuItem};
    use crate::bilibili::QualityTier;
    use crate::player::QualitySpec;

    pub(crate) fn spec(qn: i64, codecid: i64, bandwidth: u64, label: &str) -> QualitySpec {
        QualitySpec {
            qn,
            codecid,
            bandwidth,
            label: label.to_string(),
            detail: format!("{qn}x{codecid} · {bandwidth}bps"),
            badge: String::new(),
        }
    }

    pub(crate) fn tier(qn: i64, locked: bool) -> QualityTier {
        QualityTier {
            qn,
            label: format!("tier-{qn}"),
            detail: format!("{qn} detail"),
            locked,
        }
    }

    #[test]
    pub(crate) fn locked_tiers_are_rows_without_a_stream() {
        let specs = vec![
            spec(80, 7, 3_252_336, "1080P"),
            spec(80, 12, 2_070_001, "1080P"),
            spec(64, 7, 1_301_438, "720P"),
        ];
        let tiers = vec![
            tier(125, true),
            tier(120, true),
            tier(116, true),
            tier(80, false),
            tier(64, false),
        ];
        let items: Vec<QualityMenuItem> =
            build_quality_menu(&specs, &tiers, Some((80, 7, 3_252_336)));
        assert_eq!(
            items.iter().map(|item| item.row.qn).collect::<Vec<_>>(),
            vec![125, 120, 116, 80, 80, 64],
            "locked tiers first, then the catalogue's own order"
        );
        assert_eq!(items[0].row.badge.as_str(), "大会员");
        assert!(items[0].row.locked);
        assert!(
            items[0].spec.is_none(),
            "a locked row never resolves to a stream"
        );
        assert_eq!(items[3].spec, Some(0));
        assert!(items[3].row.current, "the exact stream is marked");
        assert!(!items[3].row.locked);
        // A tier that already has stream rows is not duplicated as locked.
        assert_eq!(items.iter().filter(|item| item.row.qn == 80).count(), 2);
    }

    /// Two clicks on a locked row in one open explain themselves once; a fresh
    /// open re-arms the explanation.
    #[test]
    pub(crate) fn locked_notice_shows_once_per_menu_open() {
        let mut notice = LockedNotice::default();
        assert!(notice.take(), "the first locked pick explains itself");
        assert!(!notice.take(), "the second pick in the same open is silent");
        assert!(!notice.take());
        notice.opened();
        assert!(notice.take(), "reopening the menu re-arms the toast");
        assert!(!notice.take());
    }
}

#[cfg(test)]
mod danmaku_lane_tests {
    use super::*;

    pub(crate) fn scroll(progress_ms: u32, text: &str, dmid: &str) -> crate::danmaku::Danmaku {
        crate::danmaku::Danmaku {
            progress_ms,
            mode: 1,
            fontsize: 25,
            color: 0xffffff,
            text: text.to_string(),
            mid_hash: String::new(),
            dmid: dmid.to_string(),
        }
    }

    pub(crate) fn track(items: Vec<crate::danmaku::Danmaku>) -> player::DanmakuTrack {
        let track = player::DanmakuTrack::new();
        track.set_items(items);
        track
    }

    pub(crate) fn lane() -> DanmakuLayer {
        let model = Rc::new(VecModel::<DanmakuItem>::from(Vec::<DanmakuItem>::new()));
        DanmakuLayer::new(Weak::default(), model, 120)
    }

    pub(crate) fn lane_x(layer: &DanmakuLayer, index: usize) -> f32 {
        layer.inner.borrow().rows[index].item.x
    }

    /// The rate follows the display and is capped at 120; the A/B override wins.
    #[test]
    pub(crate) fn frame_rate_caps_at_120_and_honours_the_override() {
        assert_eq!(danmaku_frame_hz_for(60, None), (60, "display"));
        assert_eq!(danmaku_frame_hz_for(120, None), (120, "display"));
        assert_eq!(danmaku_frame_hz_for(144, None), (120, "display"));
        assert_eq!(danmaku_frame_hz_for(240, None), (120, "display"));
        // A display that reports "hardware default" is not asked for 1 Hz.
        assert_eq!(danmaku_frame_hz_for(0, None), (30, "display"));
        assert_eq!(danmaku_frame_hz_for(59, None), (59, "display"));
        // The A/B override is exact, even on a 60 Hz panel.
        assert_eq!(danmaku_frame_hz_for(60, Some("120")), (120, "env"));
        assert_eq!(danmaku_frame_hz_for(120, Some("30")), (30, "env"));
        assert_eq!(danmaku_frame_hz_for(120, Some(" 60 ")), (60, "env"));
        // Anything else falls back to the display.
        assert_eq!(danmaku_frame_hz_for(120, Some("144")), (120, "display"));
        assert_eq!(danmaku_frame_hz_for(120, Some("fast")), (120, "display"));
    }

    /// 120 Hz is 8.333 ms, which Slint cannot express: the wake is 8 ms and the
    /// exact-rate gate drops the surplus.
    #[test]
    pub(crate) fn timer_wakes_are_whole_milliseconds() {
        assert_eq!(danmaku_wake_ms(120), 8);
        assert_eq!(danmaku_wake_ms(60), 16);
        assert_eq!(danmaku_wake_ms(30), 33);
        assert!((danmaku_frame_interval_ms(120) - 8.333_333).abs() < 0.001);
        assert!((danmaku_frame_interval_ms(60) - 16.666_668).abs() < 0.001);
    }

    /// 2.5 ms averaged over 2 s, three windows in a row, drops once.
    #[test]
    pub(crate) fn budget_guard_drops_after_three_bad_windows() {
        let start = Instant::now();
        // One sample inside the window and the one that closes it: every window
        // then reports exactly `cost`.
        fn window(
            guard: &mut BudgetGuard,
            start: Instant,
            step: u32,
            cost: u64,
        ) -> Option<BudgetWindow> {
            let span = DANMAKU_BUDGET_WINDOW;
            let seed = Duration::from_millis(1);
            if step == 1 {
                guard.sample(start + seed, cost);
            } else {
                guard.sample(start + span * (step - 1) + seed, cost);
            }
            guard.sample(start + span * step, cost)
        }
        // Two bad windows, then a good one: the streak resets.
        let mut guard = BudgetGuard::new(start);
        assert!(!window(&mut guard, start, 1, 9_000).unwrap().drop);
        assert!(!window(&mut guard, start, 2, 9_000).unwrap().drop);
        assert!(!window(&mut guard, start, 3, 400).unwrap().drop);
        assert!(!window(&mut guard, start, 4, 9_000).unwrap().drop);
        // Three in a row drops, and only once.
        let mut guard = BudgetGuard::new(start);
        for step in 1..=3u32 {
            let report = window(&mut guard, start, step, 9_000).unwrap();
            assert_eq!(report.drop, step == 3);
        }
        assert!(
            !window(&mut guard, start, 4, 9_000).unwrap().drop,
            "the valve drops once and latches"
        );
        // 2.4 ms is inside the budget.
        let mut guard = BudgetGuard::new(start);
        for step in 1..=6u32 {
            let report = window(&mut guard, start, step, 2_400).unwrap();
            assert!(!report.drop);
            assert_eq!(report.avg_us, 2_400);
        }
        // A window with no ticks at all (the lane was frozen) is not judged.
        let mut guard = BudgetGuard::new(start);
        let idle = guard.sample(start + DANMAKU_BUDGET_WINDOW * 3, 9_000);
        assert!(idle.is_none(), "an idle window has no average");
    }

    /// The lane's x must be the engine's x: same formula, same operand order.
    #[test]
    pub(crate) fn fast_lane_matches_the_engine_pixel_for_pixel() {
        let filter = player::DanmakuFilter::default();
        let base = Instant::now();
        let scroll_track = track(vec![scroll(1_000, "这是一条滚动弹幕 danmaku", "d1")]);
        let scroll_lane = lane();
        let published = scroll_track.compute(1_050, 20.0, &filter);
        assert_eq!(published.len(), 1);
        let engine_at_publish = published[0].x;
        scroll_lane.publish(1_050.0, published, 1.0, 100, true, base);
        // The lane starts exactly where the engine put the comment ...
        assert_eq!(lane_x(&scroll_lane, 0), engine_at_publish);
        // ... and 40 ms later it is within a pixel of the engine there.
        scroll_lane.tick(base + Duration::from_millis(40));
        let engine_later = scroll_track.compute(1_090, 20.0, &filter)[0].x;
        assert!(
            (lane_x(&scroll_lane, 0) - engine_later).abs() <= 1.0,
            "lane {} vs engine {engine_later}",
            lane_x(&scroll_lane, 0)
        );
        // A 顶部 comment (mode 5) never moves, however many frames pass.
        let top_track = track(vec![crate::danmaku::Danmaku {
            mode: 5,
            ..scroll(1_000, "顶部弹幕", "d2")
        }]);
        let top_lane = lane();
        let top = top_track.compute(1_050, 20.0, &filter);
        assert_eq!(top.len(), 1);
        top_lane.publish(1_050.0, top, 1.0, 100, true, base);
        let fixed_x = lane_x(&top_lane, 0);
        for step in 1..=10u64 {
            top_lane.tick(base + Duration::from_millis(step * 40));
        }
        assert_eq!(lane_x(&top_lane, 0), fixed_x);
    }

    /// A 200 ms stall may move the comments 100 ms, then catches up - it never
    /// teleports, and the media time it lands on is exact.
    #[test]
    pub(crate) fn stall_is_capped_and_then_caught_up() {
        let filter = player::DanmakuFilter::default();
        let base = Instant::now();
        let track = track(vec![scroll(0, "卡顿测试", "d1")]);
        let lane = lane();
        lane.publish(
            1_000.0,
            track.compute(1_000, 20.0, &filter),
            1.0,
            100,
            true,
            base,
        );
        // Frame at +250 ms: 250 ms of media time, but only 100 ms of travel.
        lane.tick(base + Duration::from_millis(250));
        assert!((lane.inner.borrow().t_ms - 1_100.0).abs() < 0.01);
        let at_1100 = track.compute(1_100, 20.0, &filter)[0].x;
        assert!((lane_x(&lane, 0) - at_1100).abs() <= 1.0);
        // ... and it does not stop there: the next frames close the gap.
        lane.tick(base + Duration::from_millis(300));
        assert!((lane.inner.borrow().t_ms - 1_200.0).abs() < 0.01);
        // 150 ms behind at this point, so one more capped step only reaches
        // 1_300 - the lane never jumps the whole distance in one frame.
        lane.tick(base + Duration::from_millis(350));
        assert!((lane.inner.borrow().t_ms - 1_300.0).abs() < 0.01);
        // ... and the frame after that is back on the media clock.
        lane.tick(base + Duration::from_millis(360));
        assert!((lane.inner.borrow().t_ms - 1_360.0).abs() < 0.01);
        let at_1360 = track.compute(1_360, 20.0, &filter)[0].x;
        assert!((lane_x(&lane, 0) - at_1360).abs() <= 1.0);
    }

    /// The clock is `anchor + wall`, not an accumulation: a tick that arrives
    /// late lands on the media time it should, without a per-tick remainder.
    #[test]
    pub(crate) fn motion_is_time_based_not_accumulated() {
        let filter = player::DanmakuFilter::default();
        let base = Instant::now();
        let track = track(vec![scroll(0, "时间基准", "d1")]);
        let lane = lane();
        lane.publish(
            1_000.0,
            track.compute(1_000, 20.0, &filter),
            1.0,
            100,
            true,
            base,
        );
        // Irregular gaps (all longer than the 8.33 ms interval, so the gate does
        // not interfere): the media time is always exactly the anchor plus the
        // wall time, not the sum of nominal intervals.
        for gap in [9u64, 22, 47, 58, 98, 114, 139] {
            lane.tick(base + Duration::from_millis(gap));
            let want = 1_000.0 + gap as f32;
            assert!((lane.inner.borrow().t_ms - want).abs() < 0.01, "gap {gap}");
        }
    }

    /// A publish reconciles by dmid: survivors keep their motion, new rows are
    /// appended, gone rows removed.
    #[test]
    pub(crate) fn publish_diffs_the_model_by_dmid() {
        let filter = player::DanmakuFilter::default();
        let base = Instant::now();
        let items = |dmids: &[&str]| {
            let raw: Vec<crate::danmaku::Danmaku> = dmids
                .iter()
                .map(|dmid| scroll(0, "弹幕", dmid))
                .collect();
            track(raw).compute(10, 20.0, &filter)
        };
        let lane = lane();
        let first = lane.publish(10.0, items(&["a", "b"]), 1.0, 100, true, base);
        assert_eq!((first.inserted, first.removed, first.visible), (2, 0, 2));
        // "b" survives, "a" leaves, "c" arrives.
        let second = lane.publish(
            1_000.0,
            items(&["b", "c"]),
            1.0,
            100,
            true,
            base + Duration::from_millis(33),
        );
        assert_eq!(second.inserted, 1);
        assert_eq!(second.removed, 1);
        assert_eq!(second.visible, 2);
        let dmids: Vec<String> = lane
            .inner
            .borrow()
            .rows
            .iter()
            .map(|row| row.item.dmid.to_string())
            .collect();
        assert_eq!(dmids, vec!["b".to_string(), "c".to_string()]);
        // A seek is a resync: the clock snaps instead of crawling.
        assert!((lane.inner.borrow().t_ms - 1_000.0).abs() < 0.01);
    }

    /// Paused / buffering / 弹幕 off freezes the clock; the next publish resumes.
    #[test]
    pub(crate) fn frozen_lane_does_not_advance() {
        let filter = player::DanmakuFilter::default();
        let base = Instant::now();
        let track = track(vec![scroll(0, "暂停", "d1")]);
        let lane = lane();
        lane.publish(
            1_000.0,
            track.compute(1_000, 20.0, &filter),
            1.0,
            100,
            true,
            base,
        );
        lane.tick(base + Duration::from_millis(40));
        let moved = lane.inner.borrow().t_ms;
        lane.freeze();
        lane.tick(base + Duration::from_millis(400));
        assert_eq!(lane.inner.borrow().t_ms, moved, "a frozen lane is still");
        // Resume: the anchor is fresh, so the comments pick up from the media
        // clock instead of jumping by the pause length.
        lane.publish(
            1_040.0,
            track.compute(1_040, 20.0, &filter),
            1.0,
            100,
            true,
            base + Duration::from_millis(400),
        );
        lane.tick(base + Duration::from_millis(410));
        assert!((lane.inner.borrow().t_ms - 1_050.0).abs() < 0.01);
    }

    /// The lane writes the row it moves, and the model row matches the lane.
    #[test]
    pub(crate) fn moved_rows_are_written_to_the_model() {
        let filter = player::DanmakuFilter::default();
        let base = Instant::now();
        let track = track(vec![scroll(0, "移动", "a")]);
        let model = Rc::new(VecModel::<DanmakuItem>::from(Vec::<DanmakuItem>::new()));
        let lane = DanmakuLayer::new(Weak::default(), model.clone(), 120);
        lane.publish(
            1_000.0,
            track.compute(1_000, 20.0, &filter),
            1.0,
            100,
            true,
            base,
        );
        assert_eq!(model.row_count(), 1);
        let before = model.row_data(0).unwrap().x;
        lane.tick(base + Duration::from_millis(9));
        let after = model.row_data(0).unwrap().x;
        assert!(after != before, "the comment moved");
        assert_eq!(after, lane_x(&lane, 0));
    }
}
