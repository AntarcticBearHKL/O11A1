//! 弹幕: fetch, filter, lane model and the Direct2D comment layer.
//!
//! The comments live on their own composition visual between the video and the
//! control overlay. That keeps the UI bars on their on-demand redraw schedule:
//! moving comments never dirty the chrome, and the video pacer keeps its 60 fps
//! grid. The layer redraws at most 60 times a second and only while there is
//! something on screen.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::Result;
use regex::Regex;
use windows::core::{w, Interface};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_BRUSH_PROPERTIES, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, ID2D1Bitmap1,
    ID2D1Device, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image, ID2D1RenderTarget,
    ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING,
    DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
};
use windows::Win32::Graphics::Dxgi::{IDXGISurface, IDXGISwapChain1};
use windows_numerics::Matrix3x2;

use crate::gpu::{present_ui, Gpu};
use crate::log;
use crate::overlay::Rect;

/// One fetched comment, already detached from the private o11a1 type.
#[derive(Clone, Debug, Default)]
pub struct DanmakuItem {
    pub progress_ms: u64,
    pub mode: u32,
    pub fontsize: u32,
    pub color: u32,
    pub text: String,
    pub mid_hash: String,
    /// Bilibili's id for the comment; kept for the future 举报/点赞 actions.
    #[allow(dead_code)]
    pub dmid: String,
}

/// `dmid`/`mid_hash` of one comment under the pointer, plus the rendered box.
#[derive(Clone, Debug, Default)]
pub struct DanmakuHit {
    pub index: usize,
    pub dmid: String,
    pub mid_hash: String,
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl DanmakuHit {
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.width * 0.5, self.y + self.height * 0.5)
    }
}

/// Left-button long-press tracker used by the 屏蔽该用户 interaction.
///
/// The caller arms it on a comment hit, forwards pointer moves, and polls
/// [`Self::due`] from the player loop. `due` fires once, so a 500 ms hold cannot
/// block the same sender twice. Moving more than the caller's slop cancels the
/// press (a drag is not a long-press).
#[derive(Debug, Default)]
pub struct LongPress {
    start: Option<Instant>,
    origin: (f32, f32),
    fired: bool,
}

impl LongPress {
    pub fn down(&mut self, now: Instant, x: f32, y: f32) {
        self.start = Some(now);
        self.origin = (x, y);
        self.fired = false;
    }

    /// Returns true when the press is still armed after the movement.
    pub fn moved(&mut self, x: f32, y: f32, slop: f32) -> bool {
        if self.start.is_none() {
            return false;
        }
        let dx = x - self.origin.0;
        let dy = y - self.origin.1;
        if dx * dx + dy * dy > slop * slop {
            self.cancel();
            false
        } else {
            true
        }
    }

    /// Fire once after `threshold`; a cancelled or already-fired press stays
    /// quiet.
    pub fn due(&mut self, now: Instant, threshold: Duration) -> bool {
        let Some(start) = self.start else {
            return false;
        };
        if self.fired || now.duration_since(start) < threshold {
            return false;
        }
        self.fired = true;
        self.start = None;
        true
    }

    pub fn cancel(&mut self) {
        self.start = None;
        self.fired = false;
    }

    pub fn active(&self) -> bool {
        self.start.is_some()
    }
}

/// Static display settings, loaded from `o11a1::settings::load()` once per
/// player. The UI does not expose a live settings panel in this migration
/// block; the persisted values are applied at start.
pub struct DanmakuSettings {
    pub opacity: f32,
    pub font_scale: f32,
    pub speed_percent: u32,
    pub area_percent: u32,
    pub block_scroll: bool,
    pub block_top: bool,
    pub block_bottom: bool,
    blocked_senders: Vec<String>,
    keywords: Vec<String>,
    regexes: Vec<Regex>,
    pub font_sizes: [f32; 3],
}

impl DanmakuSettings {
    pub fn load(window_height: u32) -> Self {
        let stored = o11a1::settings::load();
        let keywords: Vec<String> = stored
            .blocked_keywords
            .iter()
            .map(|word| word.trim().to_lowercase())
            .filter(|word| !word.is_empty())
            .collect();
        let mut invalid_regex = 0usize;
        let regexes: Vec<Regex> = stored
            .blocked_regex
            .iter()
            .filter_map(|pattern| match Regex::new(pattern) {
                Ok(regex) => Some(regex),
                Err(_) => {
                    invalid_regex += 1;
                    None
                }
            })
            .collect();
        let font_scale = match stored.danmaku_font {
            0 => 0.85,
            2 => 1.2,
            _ => 1.0,
        };
        let area_percent = o11a1::settings::clamp_area(stored.danmaku_area);
        let font_sizes = font_sizes(window_height, font_scale);
        let settings = Self {
            opacity: stored.danmaku_opacity.min(100) as f32 / 100.0,
            font_scale,
            speed_percent: stored.danmaku_speed.clamp(50, 200),
            area_percent,
            block_scroll: stored.block_scroll,
            block_top: stored.block_top,
            block_bottom: stored.block_bottom,
            blocked_senders: stored.blocked_senders,
            keywords,
            regexes,
            font_sizes,
        };
        log::line(&format!(
            "danmaku settings opacity={} font={} speed={} area={} block={}/{}/{} keywords={} regex={} invalid_regex={invalid_regex}",
            stored.danmaku_opacity.min(100),
            stored.danmaku_font,
            settings.speed_percent,
            area_percent,
            u8::from(settings.block_scroll),
            u8::from(settings.block_top),
            u8::from(settings.block_bottom),
            settings.keywords.len(),
            settings.regexes.len(),
        ));
        settings
    }

    fn font_size_for(&self, hint: u32) -> f32 {
        match bucket_for_hint(hint) {
            0 => self.font_sizes[0],
            1 => self.font_sizes[1],
            _ => self.font_sizes[2],
        }
    }

    fn blocked(&self, item: &DanmakuItem) -> bool {
        match item.mode {
            1..=3 if self.block_scroll => return true,
            5 if self.block_top => return true,
            4 if self.block_bottom => return true,
            _ => {}
        }
        if !item.mid_hash.is_empty()
            && self
                .blocked_senders
                .iter()
                .any(|sender| sender == &item.mid_hash)
        {
            return true;
        }
        if !self.keywords.is_empty() {
            let lowered = item.text.to_lowercase();
            if self.keywords.iter().any(|word| lowered.contains(word)) {
                return true;
            }
        }
        self.regexes.iter().any(|regex| regex.is_match(&item.text))
    }
}

/// 18/25/36 API hints → three buckets. The exact pixels are computed from the
/// window height: ~22/27/32 px at 1080p, times the persisted font scale.
fn bucket_for_hint(hint: u32) -> usize {
    match hint {
        0..=18 => 0,
        19..=25 => 1,
        _ => 2,
    }
}

fn font_sizes(window_height: u32, font_scale: f32) -> [f32; 3] {
    let scale = (window_height.max(240) as f32 / 1080.0) * font_scale;
    [
        (22.0 * scale).max(12.0),
        (27.0 * scale).max(13.0),
        (32.0 * scale).max(14.0),
    ]
}

/// Fetch every segment with the session `source.rs` already resolved. Called
/// from a background thread in `Player::open`; the player thread only receives
/// the finished list.
pub fn fetch_with_session(
    session: std::sync::Arc<o11a1::bilibili::Session>,
    cid: u64,
    duration_ms: u64,
) -> Vec<DanmakuItem> {
    let Some(runtime) = o11a1::bilibili::shared_runtime() else {
        log::line("danmaku fetch failed reason=no-runtime");
        return Vec::new();
    };
    let duration_secs = duration_ms.div_ceil(1000).max(1);
    log::line(&format!(
        "danmaku fetch start cid={cid} duration_s={duration_secs}"
    ));
    let list = runtime.block_on(session.danmaku_all(cid, duration_secs));
    let items: Vec<DanmakuItem> = list
        .into_iter()
        .filter(|entry| !entry.text.trim().is_empty())
        .map(convert_item)
        .collect();
    log::line(&format!(
        "danmaku fetch ok cid={cid} n={}",
        items.len()
    ));
    if items.len() <= 64 {
        let times: Vec<String> = items
            .iter()
            .map(|item| format!("{:.1}/{}", item.progress_ms as f64 / 1000.0, item.mode))
            .collect();
        log::line(&format!("danmaku times_s=[{}]", times.join(",")));
    }
    items
}

/// Convert one core `Danmaku` into the player's detached draw item.
pub fn convert_item(entry: o11a1::danmaku::Danmaku) -> DanmakuItem {
    DanmakuItem {
        progress_ms: entry.progress_ms as u64,
        mode: entry.mode,
        fontsize: entry.fontsize,
        color: entry.color,
        text: entry.text.replace(['\r', '\n'], " ").trim().to_string(),
        mid_hash: entry.mid_hash,
        dmid: entry.dmid,
    }
}

/// The rectangle comments are allowed to occupy, in client pixels.
#[derive(Clone, Copy, Debug, Default)]
pub struct Band {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    /// Bottom edge above the control panel (bottom comments stop here).
    pub bottom_limit: f32,
    pub window_height: u32,
}

struct Active {
    index: usize,
    row: u32,
    start_ms: u64,
    end_ms: u64,
}

/// A ready-to-draw comment. Borrowed text keeps the 60 Hz path allocation-free
/// apart from the model's own active list.
pub struct RenderItem<'a> {
    pub index: usize,
    pub text: &'a str,
    pub x: f32,
    pub y: f32,
    pub bucket: usize,
    pub color: u32,
}

/// The comment model: fetched list, precomputed lanes and the active set for a
/// video clock position. The clock is the caller's (audio-master) position, so
/// pause/resume and seek need no internal timer.
pub struct DanmakuModel {
    settings: DanmakuSettings,
    items: Vec<DanmakuItem>,
    placed: Vec<Option<u32>>,
    active: Vec<Active>,
    cursor: usize,
    last_position: u64,
    band: Band,
    row_height: f32,
    rows: usize,
    scroll_ms: f32,
    last_spawned: usize,
    density: bool,
}

impl DanmakuModel {
    pub fn new(settings: DanmakuSettings, band: Band) -> Self {
        let mut model = Self {
            settings,
            items: Vec::new(),
            placed: Vec::new(),
            active: Vec::new(),
            cursor: 0,
            last_position: 0,
            band,
            row_height: 1.0,
            rows: 1,
            scroll_ms: 8000.0,
            last_spawned: 0,
            density: true,
        };
        model.relayout(band);
        model
    }

    pub fn set_items(&mut self, mut items: Vec<DanmakuItem>) {
        items.sort_by_key(|item| item.progress_ms);
        self.items = items;
        self.relayout(self.band);
        self.reseed(self.last_position);
    }

    /// Add one local comment (the one this account just sent) without a
    /// network round-trip. Its clock is the current picture position.
    pub fn push_item(&mut self, item: DanmakuItem) {
        self.items.push(item);
        self.items.sort_by_key(|item| item.progress_ms);
        self.relayout(self.band);
        self.reseed(self.last_position);
    }

    /// Update the allowed band after a window/video resize. Comments in flight
    /// are cleared; the next ones are placed against the new geometry.
    pub fn relayout(&mut self, band: Band) {
        self.band = band;
        self.settings.font_sizes = font_sizes(band.window_height, self.settings.font_scale);
        self.row_height = self.settings.font_sizes[2] * 1.32 + 2.0;
        let area_height = (band.height * self.settings.area_percent as f32 / 100.0).max(1.0);
        self.rows = ((area_height / self.row_height).floor() as usize).clamp(1, 64);
        self.scroll_ms = 8000.0 * 100.0 / self.settings.speed_percent.clamp(50, 200) as f32;
        self.place();
        self.active.clear();
        self.reseed(self.last_position);
    }

    /// Apply the 弹幕设置 popup values without rebuilding the player.
    pub fn reconfigure(
        &mut self,
        opacity: f32,
        font_scale: f32,
        speed_percent: u32,
        area_percent: u32,
        block_scroll: bool,
        block_top: bool,
        block_bottom: bool,
    ) {
        self.settings.opacity = opacity.clamp(0.0, 1.0);
        self.settings.font_scale = font_scale;
        self.settings.speed_percent = speed_percent.clamp(50, 200);
        self.settings.area_percent = area_percent.clamp(25, 100);
        self.settings.block_scroll = block_scroll;
        self.settings.block_top = block_top;
        self.settings.block_bottom = block_bottom;
        self.relayout(self.band);
    }

    pub fn set_density(&mut self, density: bool) {
        self.density = density;
    }

    /// Clear in-flight comments and start spawning at `position_ms`; used by
    /// seek. No comment whose `progress_ms` is before the target is replayed.
    pub fn reseed(&mut self, position_ms: u64) {
        self.active.clear();
        self.cursor = self
            .items
            .partition_point(|item| item.progress_ms < position_ms);
        self.last_position = position_ms;
        self.last_spawned = 0;
    }

    pub fn clear(&mut self) {
        self.active.clear();
        self.cursor = self.items.len();
        self.last_spawned = 0;
    }

    pub fn total(&self) -> usize {
        self.items.len()
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn settings(&self) -> &DanmakuSettings {
        &self.settings
    }

    #[allow(dead_code)]
    pub fn items(&self) -> &[DanmakuItem] {
        &self.items
    }

    /// Topmost rendered comment containing `(x, y)` at `position_ms`.
    ///
    /// The draw list is rebuilt from the same model the layer draws, so the
    /// hit box uses the same x/y the user sees (scrolling comments included).
    /// Later entries are drawn last and therefore win overlapping hits.
    pub fn hit_test(&self, position_ms: u64, x: f32, y: f32) -> Option<DanmakuHit> {
        let frame = self.frame(position_ms);
        for render in frame.iter().rev() {
            let Some(item) = self.items.get(render.index) else {
                continue;
            };
            let font_px = self.settings.font_size_for(item.fontsize);
            let width = estimate_width(&item.text, font_px);
            let height = font_px * 1.35 + 2.0;
            if x < render.x || x > render.x + width || y < render.y || y > render.y + height {
                continue;
            }
            return Some(DanmakuHit {
                index: render.index,
                dmid: item.dmid.clone(),
                mid_hash: item.mid_hash.clone(),
                text: item.text.clone(),
                x: render.x,
                y: render.y,
                width,
                height,
            });
        }
        None
    }

    /// Runtime-only block: add `mid_hash` to the filter and drop that sender's
    /// comments from the active set. `place` is intentionally not recomputed;
    /// keeping the other comments in flight is what makes the interaction feel
    /// like "remove this user's text", not "clear the screen".
    pub fn block_sender(&mut self, mid_hash: &str) -> usize {
        if mid_hash.is_empty() {
            return 0;
        }
        if !self
            .settings
            .blocked_senders
            .iter()
            .any(|sender| sender == mid_hash)
        {
            self.settings.blocked_senders.push(mid_hash.to_string());
        }
        let before = self.active.len();
        self.active.retain(|active| {
            self.items
                .get(active.index)
                .map(|item| item.mid_hash != mid_hash)
                .unwrap_or(false)
        });
        before.saturating_sub(self.active.len())
    }

    /// Remove a sender from the runtime filter. Existing history/live entries
    /// that were skipped by `place` come back on the next `set_items` (the undo
    /// path reloads the list); this only stops future comments being filtered.
    pub fn unblock_sender(&mut self, mid_hash: &str) {
        self.settings
            .blocked_senders
            .retain(|sender| sender != mid_hash);
    }

    #[allow(dead_code)]
    pub fn blocked_senders(&self) -> &[String] {
        &self.settings.blocked_senders
    }

    pub fn font_sizes(&self) -> [f32; 3] {
        self.settings.font_sizes
    }

    /// Advance the stream to `position_ms`, spawning due comments and retiring
    /// expired ones. Returns whether the visible set changed.
    pub fn update(&mut self, position_ms: u64) -> bool {
        if self.items.is_empty() {
            return false;
        }
        // A backwards jump that did not go through `reseed` (should not happen)
        // is treated as a seek.
        if position_ms + 250 < self.last_position {
            self.reseed(position_ms);
            return true;
        }
        let mut changed = false;
        let before = self.active.len();
        self.active.retain(|active| {
            let keep = position_ms < active.end_ms;
            if !keep {
                changed = true;
            }
            keep
        });
        while self.cursor < self.items.len()
            && self.items[self.cursor].progress_ms <= position_ms
        {
            let index = self.cursor;
            self.cursor += 1;
            let item = &self.items[index];
            if self.settings.blocked(item) {
                continue;
            }
            let Some(row) = self.placed[index] else {
                continue;
            };
            let end_ms = match item.mode {
                1..=3 => item.progress_ms + self.scroll_ms as u64,
                4 | 5 => item.progress_ms + FIXED_MS,
                _ => continue,
            };
            if end_ms <= position_ms {
                continue;
            }
            // Density guard: a burst of hundreds of identical comments must
            // not turn the picture into a wall of text. Two comments per lane
            // keeps the stream readable while preserving the lane order.
            let max_active = if self.density {
                (self.rows.max(1) * 2).max(12)
            } else {
                (self.rows.max(1) * 4).max(24)
            };
            if self.active.len() >= max_active {
                continue;
            }
            self.active.push(Active {
                index,
                row,
                start_ms: item.progress_ms,
                end_ms,
            });
            self.last_spawned = self.last_spawned.saturating_add(1);
            changed = true;
        }
        if self.active.len() != before {
            changed = true;
        }
        self.last_position = position_ms;
        changed
    }

    /// Build the draw list for `position_ms`. The text borrow is valid for the
    /// duration of one layer draw.
    pub fn frame(&self, position_ms: u64) -> Vec<RenderItem<'_>> {
        let mut out = Vec::with_capacity(self.active.len());
        for active in &self.active {
            if position_ms >= active.end_ms {
                continue;
            }
            let Some(item) = self.items.get(active.index) else {
                continue;
            };
            let font_px = self.settings.font_size_for(item.fontsize);
            let width = estimate_width(&item.text, font_px);
            let bucket = bucket_for_hint(item.fontsize);
            let row = active.row as f32;
            let (x, y) = match item.mode {
                1..=3 => {
                    let elapsed = position_ms.saturating_sub(active.start_ms) as f32;
                    let x = self.band.left
                        + self.band.width
                        - (self.band.width + width) * (elapsed / self.scroll_ms.max(1.0));
                    (x, self.band.top + row * self.row_height + 2.0)
                }
                5 => (
                    self.band.left + (self.band.width - width) * 0.5,
                    self.band.top + row * self.row_height + 2.0,
                ),
                4 => (
                    self.band.left + (self.band.width - width) * 0.5,
                    self.band.bottom_limit - (row + 1.0) * self.row_height - 2.0,
                ),
                _ => continue,
            };
            out.push(RenderItem {
                index: active.index,
                text: &item.text,
                x,
                y,
                bucket,
                color: if item.color == 0 {
                    0xFFFFFF
                } else {
                    item.color & 0x00FF_FFFF
                },
            });
        }
        out
    }

    fn place(&mut self) {
        self.placed.clear();
        self.placed.resize(self.items.len(), None);
        if self.items.is_empty() {
            return;
        }
        let rows = self.rows.max(1);
        let mut scroll_free = vec![0u64; rows];
        let mut top_free = vec![0u64; rows];
        let mut bottom_free = vec![0u64; rows];
        let scroll_ms = self.scroll_ms.max(1.0);
        let width = self.band.width.max(1.0);
        for (index, item) in self.items.iter().enumerate() {
            if self.settings.blocked(item) {
                continue;
            }
            let progress = item.progress_ms;
            let font_px = self.settings.font_size_for(item.fontsize);
            match item.mode {
                1..=3 => {
                    let text_width = estimate_width(&item.text, font_px);
                    let enter_ms =
                        ((text_width / (width + text_width)) * scroll_ms).max(600.0) as u64;
                    let mut chosen = None;
                    for (row, free) in scroll_free.iter_mut().enumerate() {
                        if *free <= progress {
                            *free = progress.saturating_add(enter_ms);
                            chosen = Some(row as u32);
                            break;
                        }
                    }
                    self.placed[index] = chosen;
                }
                5 => {
                    let mut chosen = None;
                    for (row, free) in top_free.iter_mut().enumerate() {
                        if *free <= progress {
                            *free = progress.saturating_add(FIXED_MS);
                            chosen = Some(row as u32);
                            break;
                        }
                    }
                    self.placed[index] = chosen;
                }
                4 => {
                    let mut chosen = None;
                    for (row, free) in bottom_free.iter_mut().enumerate() {
                        if *free <= progress {
                            *free = progress.saturating_add(FIXED_MS);
                            chosen = Some(row as u32);
                            break;
                        }
                    }
                    self.placed[index] = chosen;
                }
                _ => {}
            }
        }
    }
}

const FIXED_MS: u64 = 4_000;

/// Conservative rendered width; CJK glyphs are roughly one em, ASCII ~0.56 em.
fn estimate_width(text: &str, size: f32) -> f32 {
    let mut width = 0.0f32;
    for ch in text.chars() {
        width += if ch.is_ascii() { size * 0.56 } else { size };
    }
    (width + 4.0).max(size)
}

// ---------------------------------------------------------------------------
// The Direct2D comment layer
// ---------------------------------------------------------------------------

/// One composition layer above the video and below the control overlay. It is
/// full-client-sized so text is rendered at native pixels; only the video band
/// is drawn, and the rest of the target stays fully transparent.
pub struct DanmakuOverlay {
    pub swap: IDXGISwapChain1,
    _d2d_factory: ID2D1Factory1,
    _d2d_device: ID2D1Device,
    d2d: ID2D1DeviceContext,
    render_target: ID2D1RenderTarget,
    dwrite: IDWriteFactory,
    formats: [Option<IDWriteTextFormat>; 3],
    layouts: HashMap<usize, IDWriteTextLayout>,
    brushes: HashMap<u32, ID2D1SolidColorBrush>,
    shadow: Option<ID2D1SolidColorBrush>,
    opacity: f32,
    font_sizes: [f32; 3],
    width: u32,
    height: u32,
    pub frames: u64,
}

impl DanmakuOverlay {
    pub fn new(
        gpu: &Gpu,
        width: u32,
        height: u32,
        font_sizes: [f32; 3],
        opacity: f32,
    ) -> Result<Self> {
        let (swap, _) = gpu.create_swapchain(
            width,
            height,
            DXGI_ALPHA_MODE_PREMULTIPLIED,
            false,
        )?;
        unsafe {
            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory::<ID2D1Factory1>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = gpu.device.cast()?;
            let d2d_device = d2d_factory.CreateDevice(&dxgi)?;
            let d2d = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            d2d.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let render_target: ID2D1RenderTarget = d2d.cast()?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut overlay = Self {
                swap,
                _d2d_factory: d2d_factory,
                _d2d_device: d2d_device,
                d2d,
                render_target,
                dwrite,
                formats: [None, None, None],
                layouts: HashMap::new(),
                brushes: HashMap::new(),
                shadow: None,
                opacity,
                font_sizes,
                width: width.max(1),
                height: height.max(1),
                frames: 0,
            };
            overlay.set_fonts(font_sizes)?;
            overlay.set_opacity(opacity)?;
            Ok(overlay)
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            return Ok(());
        }
        unsafe {
            let _ = self.d2d.SetTarget(None::<&ID2D1Image>);
            self.swap.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }
        self.width = width;
        self.height = height;
        self.layouts.clear();
        Ok(())
    }

    pub fn set_fonts(&mut self, sizes: [f32; 3]) -> Result<()> {
        if sizes == self.font_sizes && self.formats.iter().all(Option::is_some) {
            return Ok(());
        }
        self.font_sizes = sizes;
        self.layouts.clear();
        for (index, size) in sizes.iter().enumerate() {
            self.formats[index] = Some(text_format(&self.dwrite, *size)?);
        }
        Ok(())
    }

    /// Drop cached layouts when the item list is replaced: the cache is keyed
    /// by index and an index would otherwise show the previous list's text.
    pub fn reset_layouts(&mut self) {
        self.layouts.clear();
    }

    pub fn set_opacity(&mut self, opacity: f32) -> Result<()> {        let opacity = opacity.clamp(0.0, 1.0);
        if (opacity - self.opacity).abs() < 0.0001 && self.shadow.is_some() {
            return Ok(());
        }
        self.opacity = opacity;
        self.brushes.clear();
        self.shadow = Some(solid(
            &self.render_target,
            D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.72 * opacity,
            },
        )?);
        Ok(())
    }

    /// Clear and present one transparent frame. Used at startup and whenever
    /// the switch goes off, so old comments cannot stay on screen.
    pub fn clear(&mut self) -> Result<()> {
        self.draw(&[], Rect::default(), true)
    }

    /// Draw one frame of comments. `clip` is the video band; text outside it is
    /// clipped. `force` draws even with no items (the clear path).
    pub fn draw(&mut self, items: &[RenderItem<'_>], clip: Rect, force: bool) -> Result<()> {
        if items.is_empty() && !force {
            return Ok(());
        }
        unsafe {
            let surface: IDXGISurface = self.swap.GetBuffer(0)?;
            let properties = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS(
                    D2D1_BITMAP_OPTIONS_TARGET.0 | D2D1_BITMAP_OPTIONS_CANNOT_DRAW.0,
                ),
                colorContext: ManuallyDrop::new(None),
            };
            let bitmap: ID2D1Bitmap1 =
                self.d2d.CreateBitmapFromDxgiSurface(&surface, Some(&properties))?;
            let image: ID2D1Image = bitmap.cast()?;
            self.d2d.SetTarget(&image);
            self.d2d.BeginDraw();
            self.d2d.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            if clip.width() > 0.0 && clip.height() > 0.0 {
                let d2d_clip = D2D_RECT_F {
                    left: clip.left,
                    top: clip.top,
                    right: clip.right,
                    bottom: clip.bottom,
                };
                self.d2d
                    .PushAxisAlignedClip(&d2d_clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            }
            if self.opacity > 0.0 {
                for item in items {
                    let Some(layout) = self.layout_for(item)? else {
                        continue;
                    };
                    let Some(shadow) = self.shadow.as_ref() else {
                        continue;
                    };
                    let offset = (self.font_sizes[item.bucket] / 22.0).clamp(1.0, 2.0);
                    let _ = self.d2d.DrawTextLayout(
                        windows_numerics::Vector2::new(item.x + offset, item.y + offset),
                        &layout,
                        shadow,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                    let brush = self.brush_for(item.color)?;
                    let _ = self.d2d.DrawTextLayout(
                        windows_numerics::Vector2::new(item.x, item.y),
                        &layout,
                        &brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            }
            if clip.width() > 0.0 && clip.height() > 0.0 {
                self.d2d.PopAxisAlignedClip();
            }
            let _ = self.d2d.EndDraw(None, None);
            let _ = self.d2d.SetTarget(None::<&ID2D1Image>);
            drop(image);
            drop(bitmap);
        }
        let _ = present_ui(&self.swap);
        self.frames = self.frames.saturating_add(1);
        Ok(())
    }

    fn layout_for(&mut self, item: &RenderItem<'_>) -> Result<Option<IDWriteTextLayout>> {
        if let Some(layout) = self.layouts.get(&item.index) {
            return Ok(Some(layout.clone()));
        }
        let Some(format) = self.formats[item.bucket].as_ref() else {
            return Ok(None);
        };
        let utf16: Vec<u16> = item.text.encode_utf16().take(512).collect();
        let layout = unsafe {
            self.dwrite
                .CreateTextLayout(&utf16, format, 4096.0, self.font_sizes[item.bucket] * 2.4)?
        };
        if self.layouts.len() > 1024 {
            self.layouts.clear();
        }
        self.layouts.insert(item.index, layout.clone());
        Ok(Some(layout))
    }

    fn brush_for(&mut self, rgb: u32) -> Result<ID2D1SolidColorBrush> {
        if let Some(brush) = self.brushes.get(&rgb) {
            return Ok(brush.clone());
        }
        let red = ((rgb >> 16) & 0xFF) as f32 / 255.0;
        let green = ((rgb >> 8) & 0xFF) as f32 / 255.0;
        let blue = (rgb & 0xFF) as f32 / 255.0;
        let brush = solid(
            &self.render_target,
            D2D1_COLOR_F {
                r: red,
                g: green,
                b: blue,
                a: self.opacity,
            },
        )?;
        if self.brushes.len() > 512 {
            self.brushes.clear();
        }
        self.brushes.insert(rgb, brush.clone());
        Ok(brush)
    }
}

fn text_format(dwrite: &IDWriteFactory, size: f32) -> Result<IDWriteTextFormat> {
    unsafe {
        let format = dwrite.CreateTextFormat(
            crate::overlay::player_font_family(),
            None,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size.max(8.0),
            w!("zh-cn"),
        )?;
        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        Ok(format)
    }
}

fn solid(d2d: &ID2D1RenderTarget, color: D2D1_COLOR_F) -> Result<ID2D1SolidColorBrush> {
    unsafe {
        Ok(d2d.CreateSolidColorBrush(
            &color,
            Some(&D2D1_BRUSH_PROPERTIES {
                opacity: 1.0,
                transform: Matrix3x2::identity(),
            }),
        )?)
    }
}

/// Helper for the background fetch thread: send once, ignore a closed player.
pub fn send_result(sender: &Sender<Vec<DanmakuItem>>, items: Vec<DanmakuItem>) {
    let _ = sender.send(items);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band() -> Band {
        Band {
            left: 0.0,
            top: 0.0,
            width: 1280.0,
            height: 720.0,
            bottom_limit: 580.0,
            window_height: 720,
        }
    }

    fn item(progress_ms: u64, text: &str) -> DanmakuItem {
        DanmakuItem {
            progress_ms,
            mode: 1,
            fontsize: 25,
            color: 0xFFFFFF,
            text: text.to_string(),
            mid_hash: String::new(),
            dmid: String::new(),
        }
    }

    #[test]
    fn font_mapping_uses_window_height_and_scale() {
        let sizes = font_sizes(1080, 1.0);
        assert!((sizes[0] - 22.0).abs() < 0.1);
        assert!((sizes[1] - 27.0).abs() < 0.1);
        assert!((sizes[2] - 32.0).abs() < 0.1);
        let small = font_sizes(1080, 0.85);
        assert!((small[1] - 22.95).abs() < 0.1);
        assert_eq!(bucket_for_hint(18), 0);
        assert_eq!(bucket_for_hint(25), 1);
        assert_eq!(bucket_for_hint(36), 2);
    }

    #[test]
    fn blocked_keywords_and_regex_filter_comments() {
        let settings = DanmakuSettings {
            opacity: 1.0,
            font_scale: 1.0,
            speed_percent: 100,
            area_percent: 100,
            block_scroll: false,
            block_top: false,
            block_bottom: false,
            blocked_senders: vec!["sender".into()],
            keywords: vec!["spoiler".into()],
            regexes: vec![Regex::new("^!!!").unwrap()],
            font_sizes: font_sizes(1080, 1.0),
        };
        assert!(settings.blocked(&item(0, "a SPOILER here")));
        assert!(settings.blocked(&item(0, "!!!boom")));
        let mut sender = item(0, "hello");
        sender.mid_hash = "sender".into();
        assert!(settings.blocked(&sender));
        assert!(!settings.blocked(&item(0, "hello")));
    }

    #[test]
    fn lane_assignment_reuses_only_after_the_right_edge_cleared() {
        let mut model = DanmakuModel::new(
            DanmakuSettings {
                opacity: 1.0,
                font_scale: 1.0,
                speed_percent: 100,
                area_percent: 25,
                block_scroll: false,
                block_top: false,
                block_bottom: false,
                blocked_senders: Vec::new(),
                keywords: Vec::new(),
                regexes: Vec::new(),
                font_sizes: font_sizes(1080, 1.0),
            },
            band(),
        );
        model.set_items(vec![item(0, "first"), item(10, "second"), item(9000, "late")]);
        // The second comment must take another lane because the first is still
        // entering; the late one may reuse lane 0.
        assert!(model.placed[0].is_some());
        assert!(model.placed[1].is_some());
        assert_ne!(model.placed[0], model.placed[1]);
        assert_eq!(model.placed[2], Some(0));
    }

    #[test]
    fn seek_reseeds_forward_and_does_not_replay_old_comments() {
        let mut model = DanmakuModel::new(
            DanmakuSettings {
                opacity: 1.0,
                font_scale: 1.0,
                speed_percent: 100,
                area_percent: 100,
                block_scroll: false,
                block_top: false,
                block_bottom: false,
                blocked_senders: Vec::new(),
                keywords: Vec::new(),
                regexes: Vec::new(),
                font_sizes: font_sizes(1080, 1.0),
            },
            band(),
        );
        model.set_items(vec![item(1_000, "a"), item(5_000, "b"), item(9_000, "c")]);
        model.reseed(6_000);
        assert_eq!(model.cursor, 2);
        model.update(6_001);
        assert_eq!(model.active_count(), 0);
        assert!(model.update(9_001));
        assert_eq!(model.active_count(), 1);
        assert_eq!(model.frame(9_001)[0].text, "c");
    }

    #[test]
    fn hit_test_finds_the_rendered_box_and_carries_action_ids() {
        let mut model = DanmakuModel::new(
            DanmakuSettings {
                opacity: 1.0,
                font_scale: 1.0,
                speed_percent: 100,
                area_percent: 100,
                block_scroll: false,
                block_top: false,
                block_bottom: false,
                blocked_senders: Vec::new(),
                keywords: Vec::new(),
                regexes: Vec::new(),
                font_sizes: font_sizes(1080, 1.0),
            },
            band(),
        );
        let mut comment = item(1_000, "屏蔽我");
        comment.dmid = "123456".into();
        comment.mid_hash = "abcdef".into();
        model.set_items(vec![comment]);
        model.reseed(1_000);
        assert!(model.update(1_000));
        let frame = model.frame(1_000);
        assert_eq!(frame.len(), 1);
        let hit = model
            .hit_test(1_000, frame[0].x + 4.0, frame[0].y + 4.0)
            .expect("first rendered comment is under the pointer");
        assert_eq!(hit.dmid, "123456");
        assert_eq!(hit.mid_hash, "abcdef");
        assert_eq!(hit.text, "屏蔽我");
        assert!(model.hit_test(1_000, 0.0, 0.0).is_none());
        // Past the right edge of the box there is nothing to hit.
        assert!(model
            .hit_test(1_000, frame[0].x + hit.width + 30.0, frame[0].y + 4.0)
            .is_none());
    }

    #[test]
    fn blocking_a_sender_removes_only_that_senders_active_comments() {
        let mut model = DanmakuModel::new(
            DanmakuSettings {
                opacity: 1.0,
                font_scale: 1.0,
                speed_percent: 100,
                area_percent: 100,
                block_scroll: false,
                block_top: false,
                block_bottom: false,
                blocked_senders: Vec::new(),
                keywords: Vec::new(),
                regexes: Vec::new(),
                font_sizes: font_sizes(1080, 1.0),
            },
            band(),
        );
        let mut mine = item(1_000, "mine");
        mine.mid_hash = "sender-a".into();
        let mut other = item(1_000, "other");
        other.mid_hash = "sender-b".into();
        model.set_items(vec![mine, other]);
        model.update(1_000);
        assert_eq!(model.active_count(), 2);
        assert_eq!(model.block_sender("sender-a"), 1);
        assert_eq!(model.active_count(), 1);
        assert!(model
            .blocked_senders()
            .iter()
            .any(|sender| sender == "sender-a"));
        // A duplicate block must not add a second settings row.
        assert_eq!(model.block_sender("sender-a"), 0);
        assert_eq!(
            model
                .blocked_senders()
                .iter()
                .filter(|sender| *sender == "sender-a")
                .count(),
            1
        );
        model.unblock_sender("sender-a");
        assert!(!model
            .blocked_senders()
            .iter()
            .any(|sender| sender == "sender-a"));
    }

    #[test]
    fn long_press_fires_once_after_threshold_and_cancels_on_drag() {
        let start = Instant::now();
        let mut press = LongPress::default();
        press.down(start, 100.0, 100.0);
        assert!(press.active());
        assert!(!press.due(start + Duration::from_millis(499), Duration::from_millis(500)));
        assert!(press.due(start + Duration::from_millis(500), Duration::from_millis(500)));
        assert!(!press.due(start + Duration::from_millis(900), Duration::from_millis(500)));
        assert!(!press.active());

        press.down(start, 100.0, 100.0);
        assert!(!press.moved(120.0, 100.0, 8.0));
        assert!(!press.active());
        assert!(!press.due(start + Duration::from_secs(1), Duration::from_millis(500)));
    }
}
