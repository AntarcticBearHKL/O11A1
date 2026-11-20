//! The Direct2D/DirectWrite overlay: title, progress bar, controls and stats.
//!
//! Everything is drawn into the premultiplied UI swapchain on demand (a state
//! change, a resize, or 5 Hz for the clock readout) — never once per video
//! frame. The video layer underneath is a separate swapchain; DirectComposition
//! blends the two.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_BEZIER_SEGMENT, D2D1_COLOR_F,
    D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_GRADIENT_STOP,
    D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1Bitmap1, ID2D1BitmapBrush, ID2D1Brush, ID2D1Device,
    ID2D1DeviceContext, ID2D1Factory1, ID2D1Geometry, ID2D1GeometrySink,
    ID2D1GradientStopCollection, ID2D1Image, ID2D1PathGeometry1, ID2D1RadialGradientBrush,
    ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
    D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL, D2D1_BITMAP_BRUSH_PROPERTIES,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
    D2D1_BITMAP_OPTIONS, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES, D2D1_BITMAP_PROPERTIES1,
    D2D1_BRUSH_PROPERTIES, D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_SOLID,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_EXTEND_MODE_CLAMP,
    D2D1_EXTEND_MODE_WRAP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA_2_2, D2D1_LINE_JOIN_ROUND,
    D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES1,
    D2D1_STROKE_TRANSFORM_TYPE_NORMAL, D2D1_SWEEP_DIRECTION_CLOCKWISE,
    D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_LINE_METRICS, DWRITE_LINE_SPACING_METHOD_UNIFORM, DWRITE_PIXEL_GEOMETRY_RGB,
    DWRITE_RENDERING_MODE_GDI_CLASSIC,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
};
use windows::Win32::Graphics::Dxgi::{IDXGISurface, IDXGISwapChain1};
use windows::Win32::Graphics::Gdi::{
    AddFontResourceExW, RemoveFontResourceExW, FONT_RESOURCE_CHARACTERISTICS, FR_NOT_ENUM,
};
use windows_numerics::{Matrix3x2, Vector2};

use crate::enhance::EnhanceMode;
use crate::format::format_time;
use crate::gdi_text;
use crate::gpu::{present_ui, AspectMode, Gpu};
use crate::interp::InterpMode;

// ------------------------------------------------------------------ layout

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub fn width(&self) -> f32 {
        (self.right - self.left).max(0.0)
    }
    pub fn height(&self) -> f32 {
        (self.bottom - self.top).max(0.0)
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }
}

/// Which right-strip page is showing.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidebarTab {
    #[default]
    Info = 0,
    Episodes = 1,
    Comments = 2,
    Performance = 3,
    Settings = 4,
    /// Legacy danmaku-history body; no longer a tab in the Rust strip.
    History = 5,
}

impl SidebarTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "信息",
            Self::Episodes => "选集",
            Self::Comments => "评论",
            Self::Performance => "性能",
            Self::Settings => "设置",
            Self::History => "历史",
        }
    }

    pub fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Episodes,
            2 => Self::Comments,
            3 => Self::Performance,
            4 => Self::Settings,
            _ => Self::Info,
        }
    }

    /// Slot in the five-tab strip. The legacy history body belongs to 评论.
    pub fn tab_index(self) -> Option<usize> {
        match self {
            Self::Info => Some(0),
            Self::Episodes => Some(1),
            Self::Comments | Self::History => Some(2),
            Self::Performance => Some(3),
            Self::Settings => Some(4),
        }
    }
}

/// The 设置 tab's rows, in draw/hit-test order.
pub const SETTINGS_ROWS: usize = 8;

/// Opening-quality choices. `0` means the highest stream the source has.
pub const DEFAULT_QUALITY_HEIGHTS: [u32; 7] = [0, 2160, 1440, 1080, 720, 480, 360];

pub fn quality_height_label(height: u32) -> &'static str {
    match height {
        0 => "最高",
        2160 => "2160P",
        1440 => "1440P",
        1080 => "1080P",
        720 => "720P",
        480 => "480P",
        360 => "360P",
        _ => "自定义",
    }
}

/// Inline 设置 menu metrics. The menu is part of the list flow: it starts at
/// the parent row's bottom, pushes every later row down by exactly this height
/// and uses the same row typography as the settings list.
pub const SETTINGS_MENU_ROW_H: f32 = 40.0;

pub fn settings_menu_height(scale: f32, count: usize) -> f32 {
    (4.0 + count as f32 * SETTINGS_MENU_ROW_H + 6.0) * scale.max(0.75)
}

/// Which 设置 submenu is open over the tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsMenu {
    /// Persistent opening-quality preference (highest / manual height).
    DefaultQuality,
    Quality,
    Audio,
    Interp,
    Enhance,
}

/// One playable 画质 row in the open menu.
#[derive(Clone, Default)]
pub struct QualityRowUi {
    pub qn: i64,
    pub codecid: i64,
    pub label: String,
    pub detail: String,
    pub locked: bool,
    pub current: bool,
}

/// One 音轨 row in the open menu.
#[derive(Clone, Default)]
pub struct AudioRowUi {
    pub index: usize,
    pub label: String,
    pub detail: String,
    pub current: bool,
}

/// One 插帧 / 画质增强 choice row in the open menu.
#[derive(Clone, Default)]
pub struct ChoiceRowUi {
    pub index: usize,
    pub label: String,
    pub detail: String,
    pub current: bool,
}

/// One decoded storyboard cell the progress hover is showing.
#[derive(Clone, Default)]
pub struct StoryboardCellUi {
    pub time_ms: u64,
    pub image: ImageData,
}

/// An RGBA image downloaded on the data thread. The overlay converts/caches it
/// as a D2D bitmap keyed by `id`; the engine never touches D3D.
#[derive(Clone, Default)]
pub struct ImageData {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

#[derive(Clone, Default)]
pub struct PageUi {
    pub cid: u64,
    pub title: String,
    pub duration: u64,
}

/// One reply in a comment's 楼中楼 thread. The list endpoint inlines a few
/// previews; the detail worker can fetch the rest page by page.
#[derive(Clone, Default)]
pub struct ReplyUi {
    pub rpid: i64,
    pub user: String,
    pub message: String,
    pub likes: u64,
    pub ctime: i64,
    pub avatar_url: String,
    pub avatar: Option<ImageData>,
}

#[derive(Clone, Default)]
pub struct CommentUi {
    pub rpid: i64,
    pub user: String,
    pub message: String,
    pub likes: u64,
    /// Unix seconds; the list shows a relative time next to the author.
    pub ctime: i64,
    pub pinned: bool,
    /// Total replies on Bilibili, including those not in `replies` yet.
    pub reply_count: i64,
    pub avatar_url: String,
    pub avatar: Option<ImageData>,
    /// Inline preview replies from the list endpoint, or the loaded thread.
    pub replies: Vec<ReplyUi>,
    /// Highest reply page loaded (0 until the thread is requested).
    pub replies_page: i64,
    pub replies_end: bool,
    pub replies_loading: bool,
    pub expanded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentAction {
    /// Expand / collapse the thread.
    Toggle,
    /// Fetch the next page of an already expanded thread.
    More,
}

#[derive(Clone, Copy, Debug)]
pub struct CommentHit {
    pub rpid: i64,
    pub action: CommentAction,
}

#[derive(Clone, Default)]
struct CommentLinkLayout {
    rect: Rect,
    action: CommentAction,
}

impl Default for CommentAction {
    fn default() -> Self {
        Self::Toggle
    }
}

#[derive(Clone, Default)]
struct ReplyRowLayout {
    reply_index: usize,
    top: f32,
    height: f32,
    lines: Vec<String>,
}

#[derive(Clone, Default)]
struct CommentRowLayout {
    rpid: i64,
    top: f32,
    height: f32,
    body_lines: Vec<String>,
    replies: Vec<ReplyRowLayout>,
    links: Vec<CommentLinkLayout>,
}

#[derive(Clone, Default)]
pub struct InfoUi {
    pub title: String,
    /// BV id for regular videos, or `av<aid>` when only the numeric id is
    /// known. Empty for a direct/local source.
    pub video_id: String,
    pub owner: String,
    pub views: u64,
    pub pubdate: i64,
    pub cover: Option<ImageData>,
    pub description: String,
    pub pages: Vec<PageUi>,
    pub current_page: usize,
}

#[derive(Clone, Default)]
pub struct ToastUi {
    pub text: String,
    pub error: bool,
}

/// One open danmaku context menu. `x`/`y` are the pointer coordinates in client
/// pixels; the geometry helper clamps the panel into the window.
#[derive(Clone, Default)]
pub struct DanmakuMenuUi {
    pub x: f32,
    pub y: f32,
    pub dmid: String,
    pub mid_hash: String,
    pub text: String,
    pub submenu: bool,
}

/// The 3 s 撤销 toast shown after 屏蔽该用户.
#[derive(Clone, Default)]
pub struct UndoToastUi {
    pub text: String,
}

pub const DM_MENU_LABELS: [&str; 4] = ["点赞", "举报", "屏蔽该用户", "复制内容"];
pub const DM_REPORT_LABELS: [&str; 5] = ["垃圾广告", "色情", "引战", "剧透", "其他"];
pub const DM_MENU_LIKE: usize = 0;
pub const DM_MENU_REPORT: usize = 1;
pub const DM_MENU_BLOCK: usize = 2;
pub const DM_MENU_COPY: usize = 3;
const DM_MENU_ROW_H: f32 = 34.0;
const DM_MENU_PAD: f32 = 4.0;
const DM_MENU_WIDTH: f32 = 158.0;
const DM_REPORT_WIDTH: f32 = 150.0;

fn menu_panel_size(rows: usize, width: f32, scale: f32) -> (f32, f32) {
    (
        width * scale,
        (rows as f32 * DM_MENU_ROW_H + DM_MENU_PAD * 2.0) * scale,
    )
}

/// Clamp the context menu into the client near `(anchor_x, anchor_y)`.
pub fn dm_menu_rect(anchor_x: f32, anchor_y: f32, width: f32, height: f32, scale: f32) -> Rect {
    let s = scale.max(0.75);
    let (menu_w, menu_h) = menu_panel_size(DM_MENU_LABELS.len(), DM_MENU_WIDTH, s);
    let margin = 8.0 * s;
    let left = (anchor_x + 6.0 * s)
        .min(width - menu_w - margin)
        .max(margin);
    let top = (anchor_y + 6.0 * s)
        .min(height - menu_h - margin)
        .max(margin);
    Rect {
        left,
        top,
        right: (left + menu_w).min(width - margin),
        bottom: (top + menu_h).min(height - margin),
    }
}

pub fn dm_menu_row(menu: Rect, index: usize, scale: f32) -> Rect {
    let s = scale.max(0.75);
    let top = menu.top + (DM_MENU_PAD + index as f32 * DM_MENU_ROW_H) * s;
    Rect {
        left: menu.left + DM_MENU_PAD * s,
        top,
        right: menu.right - DM_MENU_PAD * s,
        bottom: top + (DM_MENU_ROW_H - 2.0) * s,
    }
}

/// Report reasons open beside the context menu, flipping left when the right
/// edge is too close.
pub fn dm_report_rect(menu: Rect, width: f32, height: f32, scale: f32) -> Rect {
    let s = scale.max(0.75);
    let (report_w, report_h) = menu_panel_size(DM_REPORT_LABELS.len(), DM_REPORT_WIDTH, s);
    let margin = 8.0 * s;
    let mut left = menu.right + 4.0 * s;
    if left + report_w > width - margin {
        left = menu.left - report_w - 4.0 * s;
    }
    let left = left.max(margin);
    let top = menu.top.min(height - report_h - margin).max(margin);
    Rect {
        left,
        top,
        right: (left + report_w).min(width - margin),
        bottom: (top + report_h).min(height - margin),
    }
}

pub fn dm_report_row(menu: Rect, index: usize, scale: f32) -> Rect {
    dm_menu_row(menu, index, scale)
}

/// Every hit region the engine and the renderer must agree on.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub scale: f32,
    pub width: f32,
    pub height: f32,
    /// The picture area: the client minus the open sidebar, with no frame inset.
    /// Fit makes the video touch two opposite edges; the plate only paints the
    /// leftover letterbox / pillarbox axis when the window and source ratios
    /// differ, so both axes never show bands at once.
    pub stage: Rect,
    /// Custom title strip; height 0 in fullscreen.
    pub top_bar: Rect,
    pub top_close: Rect,
    /// Right strip (360 logical px) when open.
    pub sidebar: Rect,
    pub sidebar_tabs: [Rect; 5],
    pub sidebar_content: Rect,
    /// 历史 tab: 实时弹幕 row and date rows start below it.
    pub history_live: Rect,
    /// Rust floating pill: seek band over a 36px control row.
    pub panel: Rect,
    /// 22px seek band inside the pill.
    pub track: Rect,
    pub loop_btn: Rect,
    pub toolbar_buttons: [Rect; 9],
    pub divider1: Rect,
    pub divider2: Rect,
    /// 播放结束行为 menu (原地暂停 / 循环播放 / 下一首或关闭 / 下一分P).
    pub loop_menu: Rect,
    pub loop_rows: [Rect; 4],
    /// 弹幕 settings popover.
    pub popup: Rect,
    /// Full first row; clicking anywhere on it toggles the danmaku layer.
    pub popup_toggle: Rect,
    /// Slider tracks (4px high), centered in their 40px rows.
    pub popup_opacity: Rect,
    pub popup_font: Rect,
    pub popup_speed: Rect,
    pub popup_area: [Rect; 4],
    pub popup_blocks: [Rect; 3],
    pub popup_density: Rect,
    pub info_actions: [Rect; 3],
    /// Info tab hero: full-width cover, title, uploader row and stats.
    pub info_cover: Rect,
    pub info_title: Rect,
    pub info_owner: Rect,
    pub info_stats: Rect,
    /// Video id field (whole row is a copy target) and its copy affordance.
    pub info_id: Rect,
    pub info_id_copy: Rect,
    /// Description body below the 简介 header.
    pub info_description: Rect,
    /// Sticky comments header: title/count on the left, sort segments right.
    pub comments_header: Rect,
    /// The two 28px sort segments (最热 / 最新).
    pub comments_sort: [Rect; 2],
    /// Retry button inside the centered error card.
    pub error_retry: Rect,
    pub stats: Rect,
    /// 3 s undo toast (屏蔽该用户) and its 撤销 button.
    pub undo_toast: Rect,
    pub undo_button: Rect,
    /// Right-click precision seek panel: heatmap, A/B loop and frame wheel.
    pub precision: Rect,
    pub precision_track: Rect,
    pub precision_a: Rect,
    pub precision_b: Rect,
    pub precision_clear: Rect,
    /// Frame-step select field and its upward dropdown.
    pub precision_step: Rect,
    pub precision_step_menu: Rect,
    pub precision_step_rows: [Rect; 4],
    pub precision_wheel: Rect,
    /// Speed panel: one full-width speed ruler, same form as the precision
    /// frame wheel.
    pub speed_panel: Rect,
    pub speed_wheel: Rect,
    pub hints_y: f32,
}

pub const TB_PRECISION: usize = 0;
pub const TB_SPEED: usize = 1;
pub const TB_MUTE: usize = 2;
pub const TB_DANMAKU: usize = 3;
pub const TB_INFO: usize = 4;
pub const TB_COMMENTS: usize = 5;
pub const TB_SERIES: usize = 6;
pub const TB_PERF: usize = 7;
pub const TB_SETTINGS: usize = 8;

impl Layout {
    pub fn new(width: f32, height: f32, scale: f32, sidebar_open: bool, fullscreen: bool) -> Self {
        let s = scale.max(0.75);
        let top_h = if fullscreen { 0.0 } else { 44.0 * s };
        // The sidebar is a full-height lane attached directly to the right of
        // the picture, not a floating card inside the window. Windowed mode
        // grows the outer window by exactly that width so the picture keeps its
        // size; fullscreen takes the lane out of the picture inward.
        let side_w = 360.0 * s;
        let reserve = if sidebar_open { side_w } else { 0.0 };
        let ui_w = (width - reserve).max(320.0 * s);
        // No uniform frame: the picture is fitted edge-to-edge in one axis and
        // the themed plate fills only the other axis when the ratios differ.
        let stage = Rect {
            left: 0.0,
            top: 0.0,
            right: ui_w.max(1.0),
            bottom: height.max(1.0),
        };
        // Window controls belong to the picture, not to the outer window with
        // the sidebar lane. The top strip therefore spans only the stage.
        let top_bar = Rect {
            left: 0.0,
            top: 0.0,
            right: ui_w.max(1.0),
            bottom: top_h,
        };
        // Rust floating pill: 10 | 22 seek | 8 | 36 controls | 10 = 86 tall.
        let bar_cell = 36.0 * s;
        let bar_gap = 8.0 * s;
        let bar_pad_x = 12.0 * s;
        let bar_pad_y = 10.0 * s;
        let bar_seek_h = 22.0 * s;
        let bar_sep_w = s;
        let bar_sep_pad = 10.0 * s;
        let bar_sep = bar_sep_pad * 2.0 + bar_sep_w;
        // Compact row: loop, precision, speed | mute, danmaku, info, comments,
        // series | perf, settings. No play cell (spacebar), no clock column,
        // no volume slider.
        let bar_content_w = bar_cell * 10.0 + bar_gap * 7.0 + bar_sep * 2.0;
        let bar_w = (bar_pad_x * 2.0 + bar_content_w).min((ui_w - 8.0 * s).max(320.0 * s));
        let bar_h = bar_pad_y * 2.0 + bar_cell + bar_seek_h + bar_gap;
        let bar_top = (height - 112.0 * s).max(0.0);
        let toolbar = Rect {
            left: (ui_w - bar_w) * 0.5,
            top: bar_top,
            right: (ui_w + bar_w) * 0.5,
            bottom: bar_top + bar_h,
        };
        let track = Rect {
            left: toolbar.left + bar_pad_x,
            top: toolbar.top + bar_pad_y,
            right: toolbar.right - bar_pad_x,
            bottom: toolbar.top + bar_pad_y + bar_seek_h,
        };
        let row_top = track.bottom + bar_gap;
        let loop_btn = Rect {
            left: toolbar.left + bar_pad_x,
            top: row_top,
            right: toolbar.left + bar_pad_x + bar_cell,
            bottom: row_top + bar_cell,
        };
        let mut toolbar_buttons = [Rect::default(); 9];
        let mut x = loop_btn.right + bar_gap;
        for (index, rect) in toolbar_buttons.iter_mut().enumerate() {
            *rect = Rect {
                left: x,
                top: row_top,
                right: x + bar_cell,
                bottom: row_top + bar_cell,
            };
            let gap = match index {
                TB_SPEED | TB_SERIES => bar_sep,
                _ => bar_gap,
            };
            x += bar_cell + gap;
        }
        let divider1 = Rect {
            left: toolbar_buttons[TB_SPEED].right + bar_sep_pad,
            top: row_top + (bar_cell - 14.0 * s) * 0.5,
            right: toolbar_buttons[TB_SPEED].right + bar_sep_pad + bar_sep_w,
            bottom: row_top + (bar_cell + 14.0 * s) * 0.5,
        };
        let divider2 = Rect {
            left: toolbar_buttons[TB_SERIES].right + bar_sep_pad,
            top: divider1.top,
            right: toolbar_buttons[TB_SERIES].right + bar_sep_pad + bar_sep_w,
            bottom: divider1.bottom,
        };
        let menu_row_h = 34.0 * s;
        let menu_bottom = toolbar.top - 12.0 * s;
        // End-of-video menu anchored over the loop cell. Four rows so the
        // pause / loop / next / next-part choices are explicit.
        let loop_menu_w = 132.0 * s;
        let loop_menu_h = menu_row_h * 4.0 + 12.0 * s;
        let loop_left = ((loop_btn.left + loop_btn.right) * 0.5 - loop_menu_w * 0.5)
            .clamp(8.0 * s, (ui_w - loop_menu_w - 8.0 * s).max(8.0 * s));
        let loop_menu = Rect {
            left: loop_left,
            top: menu_bottom - loop_menu_h,
            right: loop_left + loop_menu_w,
            bottom: menu_bottom,
        };
        let mut loop_rows = [Rect::default(); 4];
        for (index, rect) in loop_rows.iter_mut().enumerate() {
            *rect = Rect {
                left: loop_menu.left + 6.0 * s,
                top: loop_menu.top + 6.0 * s + index as f32 * menu_row_h,
                right: loop_menu.right - 6.0 * s,
                bottom: loop_menu.top + 6.0 * s + (index + 1) as f32 * menu_row_h,
            };
        }
        // Full height of the video window, flush with its right edge.
        let sidebar = Rect {
            left: width - side_w,
            top: 0.0,
            right: width,
            bottom: height,
        };
        let tab_left = sidebar.left + 18.0 * s;
        let tab_top = 12.0 * s;
        // Five equal tabs across the content width; the old close cell is gone.
        let tab_w = ((side_w - 36.0 * s) / 5.0).max(48.0 * s);
        let mut sidebar_tabs = [Rect::default(); 5];
        for (index, rect) in sidebar_tabs.iter_mut().enumerate() {
            let left = tab_left + index as f32 * tab_w;
            *rect = Rect {
                left,
                top: tab_top,
                right: left + tab_w,
                bottom: tab_top + 36.0 * s,
            };
        }
        let sidebar_content = Rect {
            left: sidebar.left + 18.0 * s,
            top: 56.0 * s,
            right: sidebar.right - 18.0 * s,
            bottom: sidebar.bottom - 16.0 * s,
        };
        let history_live = Rect {
            left: sidebar_content.left,
            top: sidebar_content.top + 30.0 * s,
            right: sidebar_content.right,
            bottom: sidebar_content.top + 62.0 * s,
        };
        let popup_w = (420.0 * s).min((ui_w - 32.0 * s).max(300.0 * s));
        // 40px title slot + seven equal 40px rows + 14px bottom padding.
        let popup_h = 334.0 * s;
        let popup_bottom = (toolbar.top - 18.0 * s).max(popup_h + 8.0 * s);
        let popup = Rect {
            left: (ui_w - popup_w) * 0.5,
            top: popup_bottom - popup_h,
            right: (ui_w + popup_w) * 0.5,
            bottom: popup_bottom,
        };
        let popup_pad = 16.0 * s;
        let row_top = popup.top + 40.0 * s;
        let row_h = 40.0 * s;
        let control_left = popup.left + 104.0 * s;
        let control_right = popup.right - popup_pad;
        let chip_h = 28.0 * s;
        let chip_gap = 8.0 * s;
        let row_rect = |index: usize| Rect {
            left: popup.left + popup_pad,
            top: row_top + index as f32 * row_h,
            right: control_right,
            bottom: row_top + (index + 1) as f32 * row_h,
        };
        let chip_top = |index: usize| row_top + index as f32 * row_h + (row_h - chip_h) * 0.5;
        let popup_toggle = row_rect(0);
        let track_top = |index: usize| row_top + index as f32 * row_h + (row_h - 4.0 * s) * 0.5;
        let popup_opacity = Rect {
            left: control_left,
            top: track_top(1),
            right: control_right - 52.0 * s,
            bottom: track_top(1) + 4.0 * s,
        };
        let popup_speed = Rect {
            left: control_left,
            top: track_top(3),
            right: control_right - 52.0 * s,
            bottom: track_top(3) + 4.0 * s,
        };
        let popup_font = Rect {
            left: control_left,
            top: track_top(2),
            right: control_right - 52.0 * s,
            bottom: track_top(2) + 4.0 * s,
        };
        let area_w = (control_right - control_left).max(1.0);
        let mut popup_area = [Rect::default(); 4];
        let area_w4 = (area_w - chip_gap * 3.0) / 4.0;
        for (index, rect) in popup_area.iter_mut().enumerate() {
            let left = control_left + index as f32 * (area_w4 + chip_gap);
            *rect = Rect {
                left,
                top: chip_top(4),
                right: left + area_w4,
                bottom: chip_top(4) + chip_h,
            };
        }
        let mut popup_blocks = [Rect::default(); 3];
        let block_w = (area_w - chip_gap * 2.0) / 3.0;
        for (index, rect) in popup_blocks.iter_mut().enumerate() {
            let left = control_left + index as f32 * (block_w + chip_gap);
            *rect = Rect {
                left,
                top: chip_top(5),
                right: left + block_w,
                bottom: chip_top(5) + chip_h,
            };
        }
        let popup_density = row_rect(6);
        let mut info_actions = [Rect::default(); 3];
        let info_cover = Rect {
            left: sidebar_content.left,
            top: sidebar_content.top,
            right: sidebar_content.right,
            bottom: sidebar_content.top
                + (sidebar_content.width() * 9.0 / 16.0)
                    .min(sidebar_content.height() * 0.24)
                    .max(120.0 * s),
        };
        let info_title_top = info_cover.bottom + 12.0 * s;
        let info_title = Rect {
            left: sidebar_content.left,
            top: info_title_top,
            right: sidebar_content.right,
            bottom: info_title_top + 58.0 * s,
        };
        let info_owner = Rect {
            left: sidebar_content.left,
            top: info_title.bottom + 8.0 * s,
            right: sidebar_content.right,
            bottom: info_title.bottom + 32.0 * s,
        };
        let info_stats = Rect {
            left: sidebar_content.left,
            top: info_owner.bottom + 14.0 * s,
            right: sidebar_content.right,
            bottom: info_owner.bottom + 54.0 * s,
        };
        let info_id = Rect {
            left: sidebar_content.left,
            top: info_stats.bottom + 12.0 * s,
            right: sidebar_content.right,
            bottom: info_stats.bottom + 50.0 * s,
        };
        let info_id_copy = Rect {
            left: info_id.right - 40.0 * s,
            top: info_id.top + 5.0 * s,
            right: info_id.right - 6.0 * s,
            bottom: info_id.bottom - 5.0 * s,
        };
        // 14px gap, a section head, then 8px to the buttons.
        let action_top = info_id.bottom + 50.0 * s;
        let action_w = (sidebar_content.width() - 12.0 * s) / 3.0;
        for (index, rect) in info_actions.iter_mut().enumerate() {
            let left = sidebar_content.left + index as f32 * (action_w + 6.0 * s);
            *rect = Rect {
                left,
                top: action_top,
                right: left + action_w,
                bottom: action_top + 40.0 * s,
            };
        }
        let info_description = Rect {
            left: sidebar_content.left,
            top: info_actions[0].bottom + 44.0 * s,
            right: sidebar_content.right,
            bottom: sidebar_content.bottom,
        };
        let comments_header = Rect {
            left: sidebar_content.left,
            top: sidebar_content.top,
            right: sidebar_content.right,
            bottom: sidebar_content.top + 48.0 * s,
        };
        let sort_top = comments_header.top + 10.0 * s;
        let sort_w = 58.0 * s;
        let sort_left = comments_header.right - sort_w * 2.0;
        let comments_sort = [
            Rect {
                left: sort_left,
                top: sort_top,
                right: sort_left + sort_w,
                bottom: sort_top + 28.0 * s,
            },
            Rect {
                left: sort_left + sort_w,
                top: sort_top,
                right: comments_header.right,
                bottom: sort_top + 28.0 * s,
            },
        ];
        let error_h = 156.0 * s;
        let error_top = (height * 0.5 - error_h * 0.5).max(top_h + 8.0 * s);
        let error_cx = (stage.left + stage.right) * 0.5;
        let error_retry = Rect {
            left: error_cx - 48.0 * s,
            top: error_top + error_h - 48.0 * s,
            right: error_cx + 48.0 * s,
            bottom: error_top + error_h - 16.0 * s,
        };
        let undo_w = (330.0 * s).min((ui_w - 24.0 * s).max(220.0 * s));
        let undo_bottom = (toolbar.top - 16.0 * s).max(64.0 * s);
        let undo_toast = Rect {
            left: (ui_w - undo_w) * 0.5,
            top: undo_bottom - 40.0 * s,
            right: (ui_w + undo_w) * 0.5,
            bottom: undo_bottom,
        };
        let undo_button = Rect {
            left: undo_toast.right - 86.0 * s,
            top: undo_toast.top + 6.0 * s,
            right: undo_toast.right - 8.0 * s,
            bottom: undo_toast.bottom - 6.0 * s,
        };
        // Precision panel: the toolbar grows upward from one fixed bottom
        // anchor. It keeps its own 626px width, independent of the compact
        // toolbar, and morphs out of the pill on the same centre line. Compact
        // vertical rhythm: 30px header, 30px heatmap, 32px A/B row and a 38px
        // wheel + frame-step band; 20px side padding, 180px tall.
        let precision_h = 180.0 * s;
        let precision_w = (626.0 * s).min((ui_w - 8.0 * s).max(320.0 * s));
        let precision_cx = (toolbar.left + toolbar.right) * 0.5;
        let precision_left = (precision_cx - precision_w * 0.5)
            .clamp(8.0 * s, (ui_w - precision_w - 8.0 * s).max(8.0 * s));
        let precision = Rect {
            left: precision_left,
            top: toolbar.bottom - precision_h,
            right: precision_left + precision_w,
            bottom: toolbar.bottom,
        };
        let p = 20.0 * s;
        let content_left = precision.left + p;
        let content_right = precision.right - p;
        let content_w = (content_right - content_left).max(1.0);
        let precision_track = Rect {
            left: content_left,
            top: precision.top + 48.0 * s,
            right: content_right,
            bottom: precision.top + 78.0 * s,
        };
        let ab_top = precision.top + 88.0 * s;
        let ab_h = 32.0 * s;
        let ab_gap = 8.0 * s;
        // A / B share the row; 清空 owns the tail as a clear pale-red action.
        // A wider minimum keeps the two-character label legible and visible at
        // every toolbar width.
        let clear_w = (content_w * 0.13).clamp(72.0 * s, content_w * 0.30);
        let ab_w = ((content_w - clear_w - ab_gap * 2.0).max(40.0)) * 0.5;
        let precision_a = Rect {
            left: content_left,
            top: ab_top,
            right: content_left + ab_w,
            bottom: ab_top + ab_h,
        };
        let precision_b = Rect {
            left: precision_a.right + ab_gap,
            top: ab_top,
            right: precision_a.right + ab_gap + ab_w,
            bottom: ab_top + ab_h,
        };
        let precision_clear = Rect {
            left: content_right - clear_w,
            top: ab_top,
            right: content_right,
            bottom: ab_top + ab_h,
        };
        let wheel_top = precision.top + 130.0 * s;
        let wheel_h = 38.0 * s;
        let step_w = 92.0 * s;
        // The frame-step select is the same height as the ruler, so the two
        // controls share one clean horizontal band.
        let precision_step = Rect {
            left: content_right - step_w,
            top: wheel_top,
            right: content_right,
            bottom: wheel_top + wheel_h,
        };
        let precision_wheel = Rect {
            left: content_left,
            top: wheel_top,
            right: precision_step.left - 12.0 * s,
            bottom: wheel_top + wheel_h,
        };
        let step_row_h = 30.0 * s;
        let step_menu_h = step_row_h * 4.0 + 8.0 * s;
        // The select opens as a compact popover to the right of the panel so
        // it never covers the frame pill or the A/B row. In a narrow window it
        // flips to the left instead of clipping at the edge.
        let menu_right = width - 8.0 * s - step_w;
        let menu_left = if precision_step.right + 8.0 * s + step_w <= width - 8.0 * s {
            precision_step.right + 8.0 * s
        } else {
            (precision_step.left - 8.0 * s - step_w).max(8.0 * s)
        };
        let precision_step_menu = Rect {
            left: menu_left.min(menu_right.max(8.0 * s)),
            top: precision_step.bottom - step_menu_h,
            right: menu_left.min(menu_right.max(8.0 * s)) + step_w,
            bottom: precision_step.bottom,
        };
        let mut precision_step_rows = [Rect::default(); 4];
        for (index, rect) in precision_step_rows.iter_mut().enumerate() {
            let top = precision_step_menu.top + 4.0 * s + index as f32 * step_row_h;
            *rect = Rect {
                left: precision_step_menu.left,
                top,
                right: precision_step_menu.right,
                bottom: top + step_row_h,
            };
        }

        // Speed panel: the toolbar itself becomes one flat horizontal picker
        // wheel. One surface, no inset chip and no second border.
        let speed_panel = toolbar;
        let speed_wheel = toolbar;
        Self {
            scale: s,
            width,
            height,
            stage,
            top_bar,
            top_close: Rect {
                left: stage.right - 8.0 * s - 36.0 * s,
                top: 8.0 * s,
                right: stage.right - 8.0 * s,
                bottom: 8.0 * s + 34.0 * s,
            },
            sidebar,
            sidebar_tabs,
            sidebar_content,
            history_live,
            panel: toolbar,
            track,
            loop_btn,
            toolbar_buttons,
            divider1,
            divider2,
            loop_menu,
            loop_rows,
            popup,
            popup_toggle,
            popup_opacity,
            popup_font,
            popup_speed,
            popup_area,
            popup_blocks,
            popup_density,
            info_actions,
            info_cover,
            info_title,
            info_owner,
            info_stats,
            info_id,
            info_id_copy,
            info_description,
            comments_header,
            comments_sort,
            error_retry,
            stats: sidebar_content,
            undo_toast,
            undo_button,
            precision,
            precision_track,
            precision_a,
            precision_b,
            precision_clear,
            precision_step,
            precision_step_menu,
            precision_step_rows,
            precision_wheel,
            speed_panel,
            speed_wheel,
            hints_y: 0.0,
        }
    }

    pub fn progress_hit(&self, x: f32, y: f32) -> bool {
        let s = self.scale;
        x >= self.track.left - 6.0 * s
            && x <= self.track.right + 6.0 * s
            && y >= self.track.top - 12.0 * s
            && y <= self.track.bottom + 12.0 * s
    }

    /// The on-screen picture rect inside the stage. Both the engine (which
    /// transforms the video swapchain) and the overlay (which paints the plate)
    /// call this, so the rounded hole and the video can never drift apart.
    pub fn stage_video_bounds(
        &self,
        video_width: u32,
        video_height: u32,
        aspect: AspectMode,
    ) -> (f32, f32, f32, f32) {
        let (x, y, width, height) = crate::gpu::video_bounds(
            self.stage.width(),
            self.stage.height(),
            video_width,
            video_height,
            aspect,
        );
        (self.stage.left + x, self.stage.top + y, width, height)
    }

    pub fn progress_fraction(&self, x: f32) -> f32 {
        ((x - self.track.left) / self.track.width().max(1.0)).clamp(0.0, 1.0)
    }

    pub fn episode_row(&self, index: usize, scroll: f32) -> Rect {
        let h = 52.0 * self.scale;
        let top = self.sidebar_content.top + 52.0 * self.scale + index as f32 * h - scroll;
        Rect {
            left: self.sidebar_content.left,
            top,
            right: self.sidebar_content.right,
            bottom: top + h - 4.0 * self.scale,
        }
    }

    /// Row `index` of the 设置 list. When `expanded_row` is open, the inline
    /// menu's height shifts every later row down.
    pub fn settings_row(
        &self,
        index: usize,
        scroll: f32,
        expanded_row: Option<usize>,
        expanded_h: f32,
    ) -> Rect {
        let h = 48.0 * self.scale;
        let shift = match expanded_row {
            Some(row) if index > row => expanded_h,
            _ => 0.0,
        };
        let top = self.sidebar_content.top + index as f32 * h - scroll + shift;
        Rect {
            left: self.sidebar_content.left,
            top,
            right: self.sidebar_content.right,
            bottom: top + h - 2.0 * self.scale,
        }
    }

    /// The inline menu directly under row `row`; its height is the space the
    /// following settings rows are shifted by.
    pub fn settings_menu_rect(&self, row: usize, count: usize, scroll: f32) -> Rect {
        let h = 48.0 * self.scale;
        let top = self.sidebar_content.top + (row + 1) as f32 * h - scroll;
        Rect {
            left: self.sidebar_content.left,
            top,
            right: self.sidebar_content.right,
            bottom: top + settings_menu_height(self.scale, count),
        }
    }

    pub fn settings_menu_option(
        &self,
        row: usize,
        count: usize,
        scroll: f32,
        index: usize,
    ) -> Rect {
        let menu = self.settings_menu_rect(row, count, scroll);
        let top = menu.top + 4.0 * self.scale + index as f32 * SETTINGS_MENU_ROW_H * self.scale;
        Rect {
            left: menu.left,
            top,
            right: menu.right,
            bottom: top + SETTINGS_MENU_ROW_H * self.scale,
        }
    }

    /// Row `index` in the 历史 tab's date list (below the 实时弹幕 row).
    pub fn history_row(&self, index: usize, scroll: f32) -> Rect {
        let h = 34.0 * self.scale;
        let top = self.history_live.bottom + 8.0 * self.scale + index as f32 * h - scroll;
        Rect {
            left: self.sidebar_content.left,
            top,
            right: self.sidebar_content.right,
            bottom: top + h - 2.0 * self.scale,
        }
    }

    /// 性能 tab: the whole row that toggles the top-right HUD. The draw and
    /// the click test both use this rect (and the tab's shared scroll).
    pub fn performance_hud_row(&self, scroll: f32) -> Rect {
        let s = self.scale;
        let top = self.sidebar_content.top + PERF_TOP_PAD * s - scroll;
        Rect {
            left: self.sidebar_content.left,
            top,
            right: self.sidebar_content.right,
            bottom: top + PERF_SWITCH_H * s,
        }
    }

    /// The hollow 54×28 switch pill on the right edge of the HUD row.
    pub fn performance_hud_switch(&self, scroll: f32) -> Rect {
        let s = self.scale;
        let row = self.performance_hud_row(scroll);
        let width = 54.0 * s;
        let height = 28.0 * s;
        let center = (row.top + row.bottom) * 0.5;
        Rect {
            left: row.right - width,
            top: center - height * 0.5,
            right: row.right,
            bottom: center + height * 0.5,
        }
    }
}

pub const HEAT_BUCKETS: usize = 120;

/// Height of the performance tab, in logical pixels. The draw and the engine
/// scroll clamp share it. There is no fixed section header any more: the HUD
/// switch and the network block scroll with the content.
pub const PERFORMANCE_CONTENT_H: f32 = 576.0;

/// 性能 tab vertical rhythm (logical pixels).
pub const PERF_TOP_PAD: f32 = 2.0;
pub const PERF_SECTION_H: f32 = 20.0;
pub const PERF_SWITCH_H: f32 = 44.0;
/// Gap + hairline + gap between the HUD switch and the network block.
pub const PERF_SECTION_GAP: f32 = 22.0;

/// End of the media span already available on this machine. A local file is
/// complete; cached remote media follows the disk cache (or the reader's
/// read-ahead window for direct URLs). Both the heat map and the toolbar cache
/// band use this so the two never disagree.
fn available_end_ms(model: &UiModel) -> u64 {
    let duration = model.duration_ms;
    if duration == 0 || model.is_live {
        return 0;
    }
    if !model.source_remote {
        return duration;
    }
    if model.total_bytes > 0 && model.loaded_bytes >= model.total_bytes {
        return duration;
    }
    let decoded_s = (model.queue_len as f64 + 1.0) / model.source_fps.max(1.0);
    let lead_s = model.net_lead_s.max(decoded_s).max(0.0);
    model
        .position_ms
        .saturating_add((lead_s * 1000.0).round() as u64)
        .min(duration)
}

/// The state the overlay draws. One value per redraw; no video pixels.
pub struct UiModel {
    pub title: String,
    pub playing: bool,
    /// 0 = pause at the end, 1 = loop, 2 = next / close.
    pub loop_mode: u8,
    pub loop_menu_open: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub volume: u8,
    pub muted: bool,
    /// A wheel notch just changed the volume: show the right-edge level bar
    /// for a moment.
    pub volume_wheel: bool,
    pub interp: InterpMode,
    /// Requested interpolation target: 0 = display refresh, 60 / 90 / 120.
    pub interp_hz: u32,
    pub enhance: EnhanceMode,
    pub speed: f32,
    /// Speed shown in the speed panel while dragging/wheeling; equals `speed`
    /// the rest of the time.
    pub speed_preview: f32,
    /// Stop in the wheel's selection window while dragging; equals
    /// `speed_preview` at rest. The labels scroll around this stop.
    pub speed_base: f32,
    /// Bottom-panel mode: 0 toolbar, 1 precision seek, 2 playback speed.
    pub panel: u8,
    pub fps: f32,
    /// Measured danmaku-layer presents per second over the last 1 s window.
    pub danmaku_fps: f32,
    /// Successful overlay presents per second. This is the whole native UI:
    /// toolbar, sidebar, HUD and every animation share it.
    pub ui_fps: f32,
    pub source_fps: f64,
    pub generated: u64,
    pub blend_us: u32,
    pub stats: bool,
    /// Media cache progress for the current source: bytes written and total.
    /// Zero for direct URLs and local files (no disk cache).
    pub loaded_bytes: u64,
    pub total_bytes: u64,
    pub status: String,
    pub source_size: (u32, u32),
    pub qn_label: String,
    pub codec: String,
    pub drag_preview: Option<u64>,
    pub precision_dragging: bool,
    pub precision_step_open: bool,
    /// Eased toolbar→precision morph, 0..1.
    pub precision_mix: f32,
    pub ab_a: Option<u64>,
    pub ab_b: Option<u64>,
    pub frame_step: u64,
    pub heatmap: [u8; HEAT_BUCKETS],
    pub tuner_offset: f32,
    pub danmaku_visible: bool,
    pub danmaku_total: usize,
    /// Live-list size while the 历史 view is showing another day's comments.
    pub live_danmaku_count: usize,
    pub danmaku_active: usize,
    pub aspect: AspectMode,
    pub ontop: bool,
    pub brightness: f32,
    pub audio_only: bool,
    pub mini: bool,
    pub mini_hint: bool,

    // Custom chrome.
    pub fullscreen: bool,
    pub chrome_shown: bool,
    pub maximized: bool,
    pub pointer: Option<(f32, f32)>,
    pub sidebar_open: bool,
    pub sidebar_tab: SidebarTab,
    pub sidebar_scroll: f32,
    pub danmaku_popup: bool,
    pub danmaku_input: String,
    pub danmaku_input_focus: bool,
    /// Right-click danmaku menu (null when closed).
    pub dm_menu: Option<DanmakuMenuUi>,
    /// 3 s undo row after blocking a sender.
    pub undo: Option<UndoToastUi>,
    pub history_dates: Vec<String>,
    pub history_dates_loading: bool,
    pub history_dates_error: Option<String>,
    pub history_date: Option<String>,
    pub history_loading: bool,
    pub history_error: Option<String>,
    pub history_count: usize,
    pub info: Option<InfoUi>,
    pub pages: Vec<PageUi>,
    pub current_page: usize,
    pub comments: Vec<CommentUi>,
    pub comments_total: i64,
    pub comments_loading: bool,
    /// 0 = 最热 (likes), 1 = 最新 (time). Animated by the engine so the sort
    /// pill glides between the two segments.
    pub comments_sort_mix: f32,
    /// 0→1 entrance after a fresh page replaces the list.
    pub comments_enter: f32,
    /// Bumped whenever the comment list or a thread changes; the overlay
    /// invalidates its measured row layout on a new value.
    pub comments_generation: u64,
    pub liked: bool,
    pub coined: bool,
    pub favorited: bool,
    pub toast: Option<ToastUi>,
    pub quality_label: String,
    /// Persistent opening-quality height (`0` = highest available).
    pub player_quality_height: u32,
    pub audio_label: String,
    /// 画质 menu rows (playable streams + locked 大会员 tiers).
    pub quality_rows: Vec<QualityRowUi>,
    /// 音轨 menu rows.
    pub audio_rows: Vec<AudioRowUi>,
    /// 插帧 menu rows (关闭 / 自动 / supported fixed rates).
    pub interp_rows: Vec<ChoiceRowUi>,
    /// 画质增强 menu rows (关闭 / 通用 / 动漫).
    pub enhance_rows: Vec<ChoiceRowUi>,
    pub settings_menu: Option<SettingsMenu>,
    pub sleep_label: String,
    pub keys_label: String,
    pub hover_preview: Option<u64>,
    pub storyboard_cell: Option<StoryboardCellUi>,
    pub light_sidebar: bool,
    /// Live stream: only live-relevant settings are shown; interp/enhance are
    /// forced off.
    pub is_live: bool,
    pub buffering: bool,
    /// The video layer has no picture yet; paint the loading backdrop.
    pub splash: bool,
    pub error: Option<String>,
    pub danmaku_opacity: u32,
    pub danmaku_font: u32,
    pub danmaku_speed: u32,
    pub danmaku_area: u32,
    pub block_scroll: bool,
    pub block_top: bool,
    pub block_bottom: bool,
    pub density_on: bool,
    pub queue_len: usize,
    pub dropped: u64,
    pub source_host: String,
    pub audio_host: String,
    pub source_remote: bool,
    /// Selected stream bitrates in bits per second; 0 when unknown.
    pub video_bitrate: u64,
    pub audio_bitrate: u64,
    /// Live ffmpeg input read rate in bytes per second (network or local).
    pub net_bps: f64,
    /// Estimated downloaded lead in seconds, from the bytes ffmpeg has read
    /// versus the media time already played.
    pub net_lead_s: f64,
    /// Active video/audio/prefetch stream processes.
    pub download_streams: usize,
}

// ------------------------------------------------------------------ renderer

fn interp_level_label(model: &UiModel) -> String {
    if model.interp == InterpMode::Off {
        "关闭".to_string()
    } else if model.interp_hz == 0 {
        "自动".to_string()
    } else {
        format!("{} Hz", model.interp_hz)
    }
}

pub struct Overlay {
    pub swap: IDXGISwapChain1,
    _d2d_factory: ID2D1Factory1,
    _d2d_device: ID2D1Device,
    d2d: ID2D1DeviceContext,
    render_target: ID2D1RenderTarget,
    dwrite: IDWriteFactory,
    title_format: IDWriteTextFormat,
    body_format: IDWriteTextFormat,
    small_format: IDWriteTextFormat,
    comment_name_format: IDWriteTextFormat,
    comment_body_format: IDWriteTextFormat,
    comment_meta_format: IDWriteTextFormat,
    wrap_comment: IDWriteTextFormat,
    hud_format: IDWriteTextFormat,
    /// Smaller face for the speed wheel's neighbour labels.
    speed_scale_format: IDWriteTextFormat,
    /// The value inside the wheel's centre window.
    speed_value_format: IDWriteTextFormat,
    /// Info tab's small section and field label.
    info_label_format: IDWriteTextFormat,
    /// Info tab's video id value (monospaced, semibold).
    info_id_format: IDWriteTextFormat,
    /// Info tab's description body, with a more generous line box.
    info_body_format: IDWriteTextFormat,
    hint_format: IDWriteTextFormat,
    precision_clock_format: IDWriteTextFormat,
    precision_frame_format: IDWriteTextFormat,
    tab_format: IDWriteTextFormat,
    tab_active_format: IDWriteTextFormat,
    /// Comment list typography: a larger body than the old 12.5px rows, with
    /// a dedicated author face and a smaller reply face.
    comment_author_format: IDWriteTextFormat,
    comment_text_format: IDWriteTextFormat,
    reply_author_format: IDWriteTextFormat,
    reply_text_format: IDWriteTextFormat,
    wrap_body: IDWriteTextFormat,
    wrap_small: IDWriteTextFormat,
    text: ID2D1SolidColorBrush,
    text_dim: ID2D1SolidColorBrush,
    text_faint: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
    panel: ID2D1SolidColorBrush,
    panel_border: ID2D1SolidColorBrush,
    chip: ID2D1SolidColorBrush,
    /// Loaded-but-not-played heat cells and the toolbar cache band.
    cache_fill: ID2D1SolidColorBrush,
    hint_band: ID2D1SolidColorBrush,
    stats_panel: ID2D1SolidColorBrush,
    heat: ID2D1SolidColorBrush,
    heat_hot: ID2D1SolidColorBrush,
    loop_fill: ID2D1SolidColorBrush,
    hud_bg: ID2D1SolidColorBrush,
    hud_text: ID2D1SolidColorBrush,
    track: ID2D1SolidColorBrush,
    chrome: ID2D1SolidColorBrush,
    chrome_soft: ID2D1SolidColorBrush,
    sidebar_line: ID2D1SolidColorBrush,
    hover: ID2D1SolidColorBrush,
    /// Active toolbar cell: only a soft accent wash. The line brush is shared
    /// with active rows in the enhance menu.
    toolbar_active: ID2D1SolidColorBrush,
    toolbar_active_line: ID2D1SolidColorBrush,
    accent_blue: ID2D1SolidColorBrush,
    accent_fg: ID2D1SolidColorBrush,
    /// One long red needle at the precision ruler's seek point.
    aim_line: ID2D1SolidColorBrush,
    danger_soft: ID2D1SolidColorBrush,
    /// Quiet pale-red fill and matching hairline for the precision 清除 action.
    danger_fill: ID2D1SolidColorBrush,
    danger_line: ID2D1SolidColorBrush,
    window_bg: ID2D1SolidColorBrush,
    window_bg_light: ID2D1SolidColorBrush,
    toast_bg: ID2D1SolidColorBrush,
    // Theme-aware sidebar brushes. The toolbar/top strip stay dark in both
    // themes; only the old white right-strip panel uses these.
    sidebar_text: ID2D1SolidColorBrush,
    sidebar_text_dim: ID2D1SolidColorBrush,
    sidebar_text_faint: ID2D1SolidColorBrush,
    sidebar_panel_bg: ID2D1SolidColorBrush,
    sidebar_panel_line: ID2D1SolidColorBrush,
    sidebar_chip: ID2D1SolidColorBrush,
    sidebar_hover: ID2D1SolidColorBrush,
    sidebar_avatar_bg: ID2D1SolidColorBrush,
    /// Home-page paper + wallpaper for the full-height sidebar lane.
    sidebar_wallpaper: ID2D1SolidColorBrush,
    sidebar_glow_light: [ID2D1RadialGradientBrush; 3],
    sidebar_glow_dark: [ID2D1RadialGradientBrush; 3],
    sidebar_dots_light: ID2D1BitmapBrush,
    sidebar_dots_dark: ID2D1BitmapBrush,
    /// Top strip stays dark in both themes, like the old player title bar.
    top_bg: ID2D1SolidColorBrush,
    top_bg_hot: ID2D1SolidColorBrush,
    top_line: ID2D1SolidColorBrush,
    top_line_hot: ID2D1SolidColorBrush,
    top_text: ID2D1SolidColorBrush,
    top_text_dim: ID2D1SolidColorBrush,
    top_cell_hot: ID2D1SolidColorBrush,
    top_close_hot: ID2D1SolidColorBrush,
    icon_stroke: ID2D1StrokeStyle,
    play_geometry: ID2D1PathGeometry1,
    bitmaps: HashMap<u64, ID2D1Bitmap1>,
    /// Hinted greyscale text masks; `None` when GDI could not initialise.
    gdi: std::cell::RefCell<Option<gdi_text::GdiText>>,
    styles: HashMap<usize, gdi_text::Style>,
    text_ok: bool,
    width: u32,
    height: u32,
    /// Swapchain allocation. It is the monitor size and never shrinks, so
    /// windowed sidebar growth and fullscreen transitions are pixel-continuous.
    surface_width: u32,
    surface_height: u32,
    layout_sidebar: bool,
    layout_fullscreen: bool,
    /// Sliding background of the sidebar tab strip (home bottom-bar style).
    tab_pill_x: f32,
    tab_pill_target: f32,
    tab_pill_ready: bool,
    /// Measured comment rows, relative to the top of the scrolling list.
    /// Rebuilt only when `UiModel::comments_generation`, the content width or
    /// the scale changes.
    comment_rows: Vec<CommentRowLayout>,
    comment_list_height: f32,
    comment_layout_key: Option<(u64, u32, u32, u32, u32)>,
    pub layout: Layout,
    pub frames: u64,
    /// Successful UI presents, used for the HUD's UI FPS line.
    presents: u64,
}

impl Overlay {
    pub fn new(
        gpu: &Gpu,
        width: u32,
        height: u32,
        surface_width: u32,
        surface_height: u32,
        scale: f32,
    ) -> Result<Self> {
        let surface_width = surface_width.max(width);
        let surface_height = surface_height.max(height);
        let (swap, _) = gpu.create_swapchain(
            surface_width,
            surface_height,
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
            if let Ok(params) = dwrite.CreateCustomRenderingParams(
                1.0,
                0.65,
                0.0,
                DWRITE_PIXEL_GEOMETRY_RGB,
                DWRITE_RENDERING_MODE_GDI_CLASSIC,
            ) {
                let _ = d2d.SetTextRenderingParams(&params);
            }
            let s = scale.max(0.75);
            let title_format = text_format(&dwrite, 16.0 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let body_format = text_format(&dwrite, 13.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            let small_format = text_format(&dwrite, 11.5 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            let comment_name_format = text_format(&dwrite, 12.5 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let comment_body_format = text_format(&dwrite, 12.5 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            let comment_meta_format = text_format(&dwrite, 12.5 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            let wrap_comment = text_format(&dwrite, 12.5 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            wrap_comment.SetWordWrapping(
                ::windows::Win32::Graphics::DirectWrite::DWRITE_WORD_WRAPPING_WRAP,
            )?;
            let hud_format = mono_format(&dwrite, 10.5 * s)?;
            let speed_scale_format = mono_format(&dwrite, 9.0 * s)?;
            let speed_value_format =
                mono_format_weight(&dwrite, 16.0 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let info_label_format =
                text_format(&dwrite, 10.5 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let info_id_format =
                mono_format_weight(&dwrite, 11.5 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let info_body_format = text_format(&dwrite, 13.5 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            info_body_format.SetWordWrapping(
                windows::Win32::Graphics::DirectWrite::DWRITE_WORD_WRAPPING_WRAP,
            )?;
            info_body_format.SetLineSpacing(
                DWRITE_LINE_SPACING_METHOD_UNIFORM,
                13.5 * s * 1.55,
                13.5 * s * 1.1,
            )?;
            // The precision header is a typographic pair, not two identical
            // pills: a large clock face for the current time, and a slightly
            // smaller semibold face for the millisecond tail and the frame
            // counter. Supporting text uses the standard small face.
            let precision_clock_format =
                text_format(&dwrite, 16.0 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let precision_frame_format =
                text_format(&dwrite, 13.0 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let hint_format = text_format(&dwrite, 11.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            hint_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            let tab_format = text_format(&dwrite, 12.5 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            tab_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            let tab_active_format = text_format(&dwrite, 12.5 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            tab_active_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            let comment_author_format =
                text_format(&dwrite, 13.5 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let comment_text_format = text_format(&dwrite, 14.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            let reply_author_format = text_format(&dwrite, 12.0 * s, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let reply_text_format = text_format(&dwrite, 13.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            let wrap_body = text_format(&dwrite, 12.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            wrap_body.SetWordWrapping(
                windows::Win32::Graphics::DirectWrite::DWRITE_WORD_WRAPPING_WRAP,
            )?;
            let wrap_small = text_format(&dwrite, 11.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            wrap_small.SetWordWrapping(
                windows::Win32::Graphics::DirectWrite::DWRITE_WORD_WRAPPING_WRAP,
            )?;

            let text = solid(&render_target, color(242, 245, 250, 1.0))?;
            let text_dim = solid(&render_target, color(205, 213, 226, 0.78))?;
            let text_faint = solid(&render_target, color(214, 221, 232, 0.55))?;
            let accent = solid(&render_target, color(251, 114, 153, 1.0))?;
            let panel = solid(&render_target, color(18, 20, 26, 0.80))?;
            let panel_border = solid(&render_target, color(255, 255, 255, 0.10))?;
            let chip = solid(&render_target, color(255, 255, 255, 0.08))?;
            let hint_band = solid(&render_target, color(6, 8, 12, 0.34))?;
            let stats_panel = solid(&render_target, color(8, 10, 15, 0.92))?;
            let heat = solid(&render_target, color(62, 166, 255, 0.55))?;
            let heat_hot = solid(&render_target, color(62, 166, 255, 0.92))?;
            let cache_fill = solid(&render_target, color(62, 166, 255, 0.30))?;
            let loop_fill = solid(&render_target, color(62, 166, 255, 0.16))?;
            let hud_bg = solid(&render_target, color(12, 14, 19, 0.88))?;
            let hud_text = solid(&render_target, color(216, 255, 223, 0.94))?;
            let track = solid(&render_target, color(255, 255, 255, 0.34))?;
            let chrome = solid(&render_target, color(18, 19, 24, 0.74))?;
            let chrome_soft = solid(&render_target, color(20, 22, 28, 0.72))?;
            let sidebar_line = solid(&render_target, color(255, 255, 255, 0.08))?;
            let hover = solid(&render_target, color(255, 255, 255, 0.12))?;
            let toolbar_active = solid(&render_target, color(62, 166, 255, 0.18))?;
            let toolbar_active_line = solid(&render_target, color(62, 166, 255, 0.55))?;
            let accent_blue = solid(&render_target, color(62, 166, 255, 1.0))?;
            let accent_fg = solid(&render_target, color(11, 11, 11, 1.0))?;
            let aim_line = solid(&render_target, color(198, 64, 58, 1.0))?;
            let danger_soft = solid(&render_target, color(208, 160, 160, 1.0))?;
            let danger_fill = solid(&render_target, color(214, 96, 84, 0.16))?;
            let danger_line = solid(&render_target, color(176, 80, 80, 0.24))?;
            let window_bg = solid(&render_target, color(15, 16, 19, 1.0))?;
            let window_bg_light = solid(&render_target, color(243, 239, 231, 1.0))?;
            let toast_bg = solid(&render_target, color(20, 22, 29, 0.88))?;
            let sidebar_text = solid(&render_target, color(242, 245, 250, 1.0))?;
            let sidebar_text_dim = solid(&render_target, color(205, 213, 226, 0.78))?;
            let sidebar_text_faint = solid(&render_target, color(214, 221, 232, 0.55))?;
            let sidebar_panel_bg = solid(&render_target, color(16, 17, 22, 0.95))?;
            let sidebar_panel_line = solid(&render_target, color(255, 255, 255, 0.08))?;
            let sidebar_chip = solid(&render_target, color(255, 255, 255, 0.08))?;
            let sidebar_hover = solid(&render_target, color(255, 255, 255, 0.12))?;
            let sidebar_avatar_bg = solid(&render_target, color(52, 58, 70, 1.0))?;
            let sidebar_wallpaper = solid(&render_target, color(15, 16, 19, 1.0))?;
            let sidebar_glow_light = [
                radial_brush(&render_target, color(255, 196, 150, 0.30))?,
                radial_brush(&render_target, color(149, 177, 255, 0.24))?,
                radial_brush(&render_target, color(178, 228, 206, 0.20))?,
            ];
            let sidebar_glow_dark = [
                radial_brush(&render_target, color(88, 54, 42, 0.30))?,
                radial_brush(&render_target, color(34, 46, 84, 0.34))?,
                radial_brush(&render_target, color(28, 60, 52, 0.22))?,
            ];
            let sidebar_dots_light = dot_brush(&render_target, 30, 26, 20, 26)?;
            let sidebar_dots_dark = dot_brush(&render_target, 255, 255, 255, 12)?;
            let top_bg = solid(&render_target, color(15, 17, 22, 0.88))?;
            let top_bg_hot = solid(&render_target, color(17, 20, 27, 0.95))?;
            let top_line = solid(&render_target, color(255, 255, 255, 0.08))?;
            let top_line_hot = solid(&render_target, color(255, 255, 255, 0.18))?;
            let top_text = solid(&render_target, color(242, 245, 250, 1.0))?;
            let top_text_dim = solid(&render_target, color(205, 213, 226, 0.78))?;
            let top_cell_hot = solid(&render_target, color(47, 91, 234, 1.0))?;
            let top_close_hot = solid(&render_target, color(213, 72, 72, 1.0))?;

            let icon_stroke: ID2D1StrokeStyle = d2d_factory
                .CreateStrokeStyle(
                    &D2D1_STROKE_STYLE_PROPERTIES1 {
                        startCap: D2D1_CAP_STYLE_ROUND,
                        endCap: D2D1_CAP_STYLE_ROUND,
                        dashCap: D2D1_CAP_STYLE_ROUND,
                        lineJoin: D2D1_LINE_JOIN_ROUND,
                        miterLimit: 10.0,
                        dashStyle: D2D1_DASH_STYLE_SOLID,
                        dashOffset: 0.0,
                        transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
                    },
                    None,
                )?
                .cast()?;
            let play_geometry: ID2D1PathGeometry1 = d2d_factory.CreatePathGeometry()?;
            let sink: ID2D1GeometrySink = play_geometry.Open()?;
            sink.BeginFigure(Vector2::new(0.0, 0.0), D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLine(Vector2::new(1.0, 0.62));
            sink.AddLine(Vector2::new(0.0, 1.24));
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close()?;

            let mut styles = HashMap::new();
            for (format, size, weight) in [
                (&title_format, 16.0 * s, 600u16),
                (&body_format, 13.0 * s, 400),
                (&small_format, 11.5 * s, 400),
                (&comment_name_format, 13.0 * s, 600),
                (&comment_body_format, 12.5 * s, 400),
                (&comment_meta_format, 12.5 * s, 400),
                (&wrap_comment, 12.5 * s, 400),
                (&hud_format, 10.5 * s, 400),
                (&info_label_format, 10.5 * s, 600),
                (&info_id_format, 11.5 * s, 600),
                (&info_body_format, 13.5 * s, 400),
                (&precision_clock_format, 16.0 * s, 600),
                (&precision_frame_format, 13.0 * s, 600),
                (&hint_format, 11.0 * s, 400),
                (&tab_format, 12.5 * s, 400),
                // The active tab is semibold, but it must stay on the same GDI
                // rasteriser and line box as the inactive tabs. Without this
                // entry it fell through to DirectWrite and silently picked the
                // system UI face, which read as the wrong font.
                (&tab_active_format, 12.5 * s, 600),
                (&comment_author_format, 13.5 * s, 600),
                (&comment_text_format, 14.0 * s, 400),
                (&reply_author_format, 12.0 * s, 600),
                (&reply_text_format, 13.0 * s, 400),
                (&wrap_body, 12.0 * s, 400),
                (&wrap_small, 11.0 * s, 400),
            ] {
                styles.insert(
                    windows::core::Interface::as_raw(format) as usize,
                    text_style(&dwrite, format, size, weight),
                );
            }
            // The description needs more air than the default 1.2 line box.
            // The GDI path reads this line height for wrapping and line
            // placement, so the whole paragraph gets the same rhythm.
            if let Some(style) = styles
                .get_mut(&(windows::core::Interface::as_raw(&info_body_format) as usize))
            {
                style.line_height = 13.5 * s * 1.55;
            }
            let text_ok = player_font_available();
            let gdi = std::cell::RefCell::new(
                if !text_ok
                    || std::env::var("O11A1_PLAYER_GDI")
                        .map(|value| value == "0")
                        .unwrap_or(false)
                {
                    None
                } else {
                    gdi_text::GdiText::new(player_font_family())
                },
            );

            Ok(Self {
                swap,
                _d2d_factory: d2d_factory,
                _d2d_device: d2d_device,
                d2d,
                render_target,
                dwrite,
                title_format,
                body_format,
                small_format,
                comment_name_format,
                comment_body_format,
                comment_meta_format,
                wrap_comment,
                hud_format,
                speed_scale_format,
                speed_value_format,
                info_label_format,
                info_id_format,
                info_body_format,
                hint_format,
                precision_clock_format,
                precision_frame_format,
                tab_format,
                tab_active_format,
                comment_author_format,
                comment_text_format,
                reply_author_format,
                reply_text_format,
                wrap_body,
                wrap_small,
                text,
                text_dim,
                text_faint,
                accent,
                panel,
                panel_border,
                chip,
                hint_band,
                stats_panel,
                heat,
                heat_hot,
                cache_fill,
                loop_fill,
                hud_bg,
                hud_text,
                track,
                chrome,
                chrome_soft,
                sidebar_line,
                hover,
                toolbar_active,
                toolbar_active_line,
                accent_blue,
                accent_fg,
                aim_line,
                danger_soft,
                danger_fill,
                danger_line,
                window_bg,
                window_bg_light,
                toast_bg,
                sidebar_text,
                sidebar_text_dim,
                sidebar_text_faint,
                sidebar_panel_bg,
                sidebar_panel_line,
                sidebar_chip,
                sidebar_hover,
                sidebar_avatar_bg,
                sidebar_wallpaper,
                sidebar_glow_light,
                sidebar_glow_dark,
                sidebar_dots_light,
                sidebar_dots_dark,
                top_bg,
                top_bg_hot,
                top_line,
                top_line_hot,
                top_text,
                top_text_dim,
                top_cell_hot,
                top_close_hot,
                icon_stroke,
                play_geometry,
                bitmaps: HashMap::new(),
                gdi,
                styles,
                text_ok,
                width,
                height,
                surface_width,
                surface_height,
                layout_sidebar: false,
                layout_fullscreen: false,
                tab_pill_x: 0.0,
                tab_pill_target: 0.0,
                tab_pill_ready: false,
                comment_rows: Vec::new(),
                comment_list_height: 0.0,
                comment_layout_key: None,
                layout: Layout::new(width as f32, height as f32, s, false, false),
                frames: 0,
                presents: 0,
            })
        }
    }

    pub fn resize(&mut self, width: u32, height: u32, scale: f32) -> Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        // Grow the allocation only when the client really exceeds it. The
        // normal sidebar open/close changes the layout but keeps the surface,
        // so no ResizeBuffers (and no blank composition frame) happens there.
        if width > self.surface_width || height > self.surface_height {
            let surface_width = width.max(self.surface_width);
            let surface_height = height.max(self.surface_height);
            unsafe {
                let _ = self.d2d.SetTarget(None::<&ID2D1Image>);
                self.swap.ResizeBuffers(
                    0,
                    surface_width,
                    surface_height,
                    DXGI_FORMAT_UNKNOWN,
                    windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG(0),
                )?;
            }
            self.surface_width = surface_width;
            self.surface_height = surface_height;
        }
        self.width = width;
        self.height = height;
        self.layout = Layout::new(
            width as f32,
            height as f32,
            scale,
            self.layout_sidebar,
            self.layout_fullscreen,
        );
        Ok(())
    }

    /// Recompute the hit regions when the right strip or fullscreen state
    /// changes. The swapchain itself does not need a resize.
    /// True while the sidebar's sliding tab pill is still easing toward the
    /// selected tab. The engine redraws at 60 Hz for the duration.
    pub fn sidebar_pill_animating(&self) -> bool {
        self.layout_sidebar
            && self.tab_pill_ready
            && (self.tab_pill_target - self.tab_pill_x).abs() > 0.4
    }

    pub fn set_chrome_state(&mut self, sidebar_open: bool, fullscreen: bool) {
        if self.layout_sidebar == sidebar_open && self.layout_fullscreen == fullscreen {
            return;
        }
        self.layout_sidebar = sidebar_open;
        self.layout_fullscreen = fullscreen;
        self.layout = Layout::new(
            self.width as f32,
            self.height as f32,
            self.layout.scale,
            sidebar_open,
            fullscreen,
        );
    }

    /// Light theme paints the old white toolbar/popup/sidebar; the top strip
    /// stays dark (its own `top_*` brushes). Dark keeps the glass chrome.
    fn apply_sidebar_theme(&self, light: bool) {
        let colors: [D2D1_COLOR_F; 8] = if light {
            [
                color(32, 33, 38, 1.0),
                color(111, 108, 102, 1.0),
                color(162, 157, 148, 1.0),
                color(243, 239, 231, 0.96),
                color(31, 28, 23, 0.10),
                color(31, 28, 23, 0.055),
                color(31, 28, 23, 0.08),
                color(221, 214, 203, 1.0),
            ]
        } else {
            [
                color(241, 238, 230, 1.0),
                color(179, 175, 166, 1.0),
                color(125, 121, 113, 1.0),
                color(15, 16, 19, 0.96),
                color(255, 255, 255, 0.09),
                color(255, 255, 255, 0.06),
                color(255, 255, 255, 0.08),
                color(39, 40, 47, 1.0),
            ]
        };
        let chrome = if light {
            [
                color(250, 248, 244, 0.66), // chrome / player-bar
                color(255, 255, 255, 0.55), // chrome_soft
                color(255, 252, 246, 0.80), // panel
                color(0, 0, 0, 0.12),       // panel_border
                color(0, 0, 0, 0.063),      // chip
                color(0, 0, 0, 0.14),       // track
                color(38, 38, 44, 1.0),     // text
                color(95, 95, 104, 1.0),    // text_dim
                color(120, 120, 130, 1.0),  // text_faint
                color(255, 252, 246, 0.94), // stats_panel
                color(255, 252, 246, 0.88), // toast_bg
                color(0, 0, 0, 0.11),       // hover
            ]
        } else {
            [
                color(18, 19, 24, 0.64),
                color(24, 26, 32, 0.55),
                color(18, 20, 26, 0.80),
                color(58, 58, 68, 0.80),
                color(42, 42, 51, 1.0),
                color(58, 58, 68, 1.0),
                color(224, 224, 230, 1.0),
                color(154, 154, 162, 1.0),
                color(154, 154, 162, 0.70),
                color(8, 10, 15, 0.92),
                color(20, 22, 29, 0.88),
                color(58, 58, 68, 1.0),
            ]
        };
        unsafe {
            let _ = self.sidebar_text.SetColor(&colors[0]);
            let _ = self.sidebar_text_dim.SetColor(&colors[1]);
            let _ = self.sidebar_text_faint.SetColor(&colors[2]);
            let _ = self.sidebar_panel_bg.SetColor(&colors[3]);
            let _ = self.sidebar_panel_line.SetColor(&colors[4]);
            let _ = self.sidebar_chip.SetColor(&colors[5]);
            let _ = self.sidebar_hover.SetColor(&colors[6]);
            let _ = self.sidebar_avatar_bg.SetColor(&colors[7]);
            let wallpaper = if light {
                color(243, 239, 231, 1.0)
            } else {
                color(15, 16, 19, 1.0)
            };
            let _ = self.sidebar_wallpaper.SetColor(&wallpaper);
            let _ = self.chrome.SetColor(&chrome[0]);
            let _ = self.chrome_soft.SetColor(&chrome[1]);
            let _ = self.panel.SetColor(&chrome[2]);
            let _ = self.panel_border.SetColor(&chrome[3]);
            let _ = self.chip.SetColor(&chrome[4]);
            let _ = self.track.SetColor(&chrome[5]);
            let _ = self.text.SetColor(&chrome[6]);
            let _ = self.text_dim.SetColor(&chrome[7]);
            let _ = self.text_faint.SetColor(&chrome[8]);
            let _ = self.stats_panel.SetColor(&chrome[9]);
            let _ = self.toast_bg.SetColor(&chrome[10]);
            let _ = self.hover.SetColor(&chrome[11]);
            let accent = if light {
                color(0, 95, 184, 1.0)
            } else {
                color(62, 166, 255, 1.0)
            };
            let accent_fg = if light {
                color(255, 255, 255, 1.0)
            } else {
                color(11, 11, 11, 1.0)
            };
            let _ = self.accent.SetColor(&accent);
            let _ = self.accent_blue.SetColor(&accent);
            let _ = self.accent_fg.SetColor(&accent_fg);
            let aim_line = if light {
                color(198, 64, 58, 1.0)
            } else {
                color(255, 112, 102, 1.0)
            };
            let _ = self.aim_line.SetColor(&aim_line);
            // Keep these three in named locals. Passing `&color(...)` straight
            // into SetColor corrupted the brush color in release builds, which
            // made the precision panel's 清空 chip render as nothing at all.
            let danger_soft = if light {
                color(176, 80, 80, 1.0)
            } else {
                color(208, 160, 160, 1.0)
            };
            let danger_fill = if light {
                color(214, 96, 84, 0.22)
            } else {
                color(230, 120, 110, 0.26)
            };
            let danger_line = if light {
                color(176, 80, 80, 0.48)
            } else {
                color(230, 130, 120, 0.55)
            };
            let _ = self.danger_soft.SetColor(&danger_soft);
            let _ = self.danger_fill.SetColor(&danger_fill);
            let _ = self.danger_line.SetColor(&danger_line);
        }
    }

    /// Clear the overlay plane to a fully transparent frame. Used by the
    /// Electron-hosted popup, where the chrome is HTML and only video/danmaku
    /// should be visible.
    pub fn clear(&mut self) -> Result<()> {
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
            let _ = self.d2d.EndDraw(None, None);
            let _ = self.d2d.SetTarget(None::<&ID2D1Image>);
            drop(image);
            drop(bitmap);
        }
        let _ = present_ui(&self.swap);
        Ok(())
    }

    pub fn draw(&mut self, model: &UiModel) -> Result<()> {
        self.draw_with(model, false)
    }

    pub fn draw_sync(&mut self, model: &UiModel) -> Result<()> {
        self.draw_with(model, true)
    }

    fn draw_with(&mut self, model: &UiModel, sync: bool) -> Result<()> {
        let layout = self.layout;
        let s = layout.scale;
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
            self.apply_sidebar_theme(model.light_sidebar);
            self.d2d.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));

            // Before the first frame the video layer has no pixels of its own.
            // Paint the entire stage as one opaque home-paper block, then the
            // picture is attached on top as soon as it arrives. Once a frame is
            // present only the letterbox/corner plate is needed.
            if model.splash {
                self.draw_home_backdrop(layout.stage, layout.stage, model.light_sidebar);
            } else {
                let (vx, vy, video_width, video_height) = layout.stage_video_bounds(
                    model.source_size.0,
                    model.source_size.1,
                    model.aspect,
                );
                let video = Rect {
                    left: vx,
                    top: vy,
                    right: vx + video_width,
                    bottom: vy + video_height,
                };
                let video_radius = if model.fullscreen || model.mini {
                    0.0
                } else {
                    16.0 * s
                };
                self.fill_stage_plate(
                    &layout,
                    video,
                    video_radius,
                    &self.sidebar_wallpaper,
                    model.light_sidebar,
                );
            }

            if !model.mini && !model.fullscreen {
                self.draw_top_strip(model, &layout);
            }
            if model.sidebar_open && !model.mini {
                self.draw_sidebar(model, &layout);
            }
            if !model.mini && (model.chrome_shown || model.precision_mix > 0.001) {
                self.draw_bottom_panel(model, &layout);
            }
            self.draw_volume_osd(model, &layout);
            if !model.mini && model.chrome_shown && model.loop_menu_open {
                self.draw_loop_menu(model, &layout);
            }
            if !model.mini && model.chrome_shown && model.danmaku_popup {
                self.draw_danmaku_popup(model, &layout);
            }

            if let Some(error) = &model.error {
                let card_w = 360.0 * s;
                let card_h = 156.0 * s;
                let cx = (layout.stage.left + layout.stage.right) * 0.5;
                let cy = (layout.stage.top + layout.stage.bottom) * 0.5;
                let card = Rect {
                    left: cx - card_w * 0.5,
                    top: (cy - card_h * 0.5).max(layout.top_bar.bottom + 8.0 * s),
                    right: cx + card_w * 0.5,
                    bottom: (cy - card_h * 0.5).max(layout.top_bar.bottom + 8.0 * s) + card_h,
                };
                self.draw_home_panel(card, 16.0 * s, card, model.light_sidebar);
                self.text_centered(
                    "播放中断",
                    Rect {
                        left: card.left + 18.0 * s,
                        top: card.top + 16.0 * s,
                        right: card.right - 18.0 * s,
                        bottom: card.top + 44.0 * s,
                    },
                    &self.tab_active_format,
                    &self.danger_soft,
                );
                self.text_wrapped(
                    &ellipsize(error, 80),
                    card.left + 20.0 * s,
                    card.top + 48.0 * s,
                    (card.width() - 40.0 * s).max(60.0),
                    44.0 * s,
                    &self.wrap_small,
                    &self.text_dim,
                );
                let retry = layout.error_retry;
                let hot = model
                    .pointer
                    .map(|(x, y)| retry.contains(x, y))
                    .unwrap_or(false);
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(retry),
                    radiusX: 8.0 * s,
                    radiusY: 8.0 * s,
                };
                self.d2d.FillRoundedRectangle(
                    &rounded,
                    if hot {
                        &self.accent
                    } else {
                        &self.accent_blue
                    },
                );
                self.text_centered("重试", retry, &self.body_format, &self.accent_fg);
            } else if model.splash || model.buffering {
                self.draw_loading_prompt(model, &layout);
            }
            if model.stats {
                self.draw_fps_hud(model, &layout);
            }

            if let Some(toast) = &model.toast {
                let toast_w =
                    (toast.text.chars().count() as f32 * 13.0 + 32.0).clamp(180.0, 520.0) * s;
                let bottom = if model.chrome_shown {
                    layout.panel.top - 18.0 * s
                } else {
                    layout.height - 24.0 * s
                };
                let rect = Rect {
                    left: (layout.width - toast_w) * 0.5,
                    top: bottom - 32.0 * s,
                    right: (layout.width + toast_w) * 0.5,
                    bottom,
                };
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(rect),
                    radiusX: 8.0 * s,
                    radiusY: 8.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.toast_bg);
                self.d2d
                    .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
                self.text_centered(
                    &toast.text,
                    rect,
                    &self.small_format,
                    if toast.error {
                        &self.danger_soft
                    } else {
                        &self.text
                    },
                );
            }

            if let Some(undo) = &model.undo {
                self.draw_undo_toast(undo, &layout);
            }
            if let Some(menu) = &model.dm_menu {
                self.draw_dm_menu(model, menu, &layout);
            }

            // Paused and finished both mean "the picture is not running".
            // The frame alone carries that state; no centered card.
            if model.status.contains("暂停") || model.status.contains("结束") {
                self.draw_pause_frame(&layout, model);
            }

            if let Err(error) = self.d2d.EndDraw(None, None) {
                crate::log::line(&format!("overlay EndDraw failed: {error:?}"));
            }
            let _ = self.d2d.SetTarget(None::<&ID2D1Image>);
            drop(image);
            drop(bitmap);
        }
        if sync {
            if crate::gpu::present_ui_sync(&self.swap) {
                self.presents = self.presents.saturating_add(1);
            }
        } else if present_ui(&self.swap) {
            self.presents = self.presents.saturating_add(1);
        }
        self.frames = self.frames.saturating_add(1);
        Ok(())
    }

    pub fn presents(&self) -> u64 {
        self.presents
    }

    /// Electron-hosted popup only: one storyboard thumbnail floating above the
    /// HTML bar. The native chrome is hidden in this mode.
    pub fn draw_storyboard_preview(&mut self, image: &ImageData, fraction: f32) -> Result<()> {
        let layout = self.layout;
        let s = layout.scale;
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
            let image_target: ID2D1Image = bitmap.cast()?;
            self.d2d.SetTarget(&image_target);
            self.d2d.BeginDraw();
            self.d2d.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            let w = 176.0 * s;
            let h = 99.0 * s;
            let stage = layout.stage;
            let left = (stage.left + (stage.width() - w) * fraction.clamp(0.0, 1.0))
                .clamp(stage.left + 8.0 * s, stage.right - w - 8.0 * s);
            let top = stage.bottom - h - 86.0 * s;
            let frame = Rect {
                left,
                top,
                right: left + w,
                bottom: top + h,
            };
            let _ = self.draw_image(image, frame);
            self.stroke_rect(frame, 1.0 * s, &self.panel_border);
            let _ = self.d2d.EndDraw(None, None);
            let _ = self.d2d.SetTarget(None::<&ID2D1Image>);
            drop(image_target);
            drop(bitmap);
        }
        let _ = present_ui(&self.swap);
        Ok(())
    }

    unsafe fn draw_fps_hud(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let width = 210.0 * s;
        let height = 91.0 * s;
        let right = if model.fullscreen {
            layout.stage.right - 12.0 * s
        } else {
            layout.stage.right - 8.0 * s
        };
        let top = if model.fullscreen {
            12.0 * s
        } else {
            layout.top_close.bottom + 6.0 * s
        };
        let rect = Rect {
            left: right - width,
            top,
            right,
            bottom: top + height,
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 7.0 * s,
            radiusY: 7.0 * s,
        };
        self.d2d
            .FillRoundedRectangle(&rounded, &self.sidebar_panel_bg);
        self.d2d
            .DrawRoundedRectangle(&rounded, &self.sidebar_panel_line, 1.0 * s, None);
        let ms = if model.fps > 0.5 {
            1000.0 / model.fps
        } else {
            0.0
        };
        let line_h = 17.0 * s;
        let mut line_top = rect.top + 3.0 * s;
        let video_bitrate = model.video_bitrate;
        let total_bps = model.video_bitrate + model.audio_bitrate;
        let network = if model.net_bps > 0.0 {
            // Actual ffmpeg read rate in bits per second.
            format!(
                "{} / {}",
                format_bitrate((model.net_bps * 8.0).round() as u64),
                format_bitrate(video_bitrate)
            )
        } else if total_bps > 0 {
            // First second after open: fall back to the stream's demand until
            // the process read counter has a baseline.
            let demand_bps = (total_bps as f64 * model.speed as f64).round() as u64;
            format!(
                "{} / {}",
                format_bitrate(demand_bps),
                format_bitrate(video_bitrate)
            )
        } else {
            "— / —".to_string()
        };
        let fps = if model.source_fps > 1.0 {
            model.source_fps
        } else {
            30.0
        };
        // Prefer the network lead (bytes read ahead of the media clock); the
        // decoded six-frame ring is the floor when the bitrate is unknown.
        let decoded = (model.queue_len as f64 + 1.0) / fps;
        let buffered = model.net_lead_s.max(decoded).min(99.9);
        let lines = [
            format!("画面 FPS {:.1} · {:.1} ms", model.fps, ms),
            format!("弹幕 FPS {:.1}", model.danmaku_fps),
            network,
            format!("缓冲 {:.1} s", buffered),
            format!("UI FPS {:.1}", model.ui_fps),
        ];
        for text in lines {
            let slot = Rect {
                left: rect.left,
                top: line_top,
                right: rect.right,
                bottom: line_top + line_h,
            };
            self.text_centered(&text, slot, &self.hud_format, &self.sidebar_text_dim);
            line_top += line_h;
        }
    }

    unsafe fn draw_top_strip(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let rect = layout.top_close;
        let hot = model
            .pointer
            .map(|(x, y)| rect.contains(x, y))
            .unwrap_or(false);
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 10.0 * s,
            radiusY: 10.0 * s,
        };
        // One close button, on the same warm paper as the sidebar and the
        // toolbar. No dark glass capsule, no minimize/maximize cells.
        self.d2d.FillRoundedRectangle(
            &rounded,
            if hot {
                &self.top_close_hot
            } else {
                &self.sidebar_panel_bg
            },
        );
        self.d2d.DrawRoundedRectangle(
            &rounded,
            if hot {
                &self.top_close_hot
            } else {
                &self.sidebar_panel_line
            },
            1.0 * s,
            None,
        );
        let ink = if hot {
            &self.top_text
        } else {
            &self.sidebar_text_dim
        };
        let cx = (rect.left + rect.right) * 0.5;
        let cy = (rect.top + rect.bottom) * 0.5;
        let k = if hot { 1.08 } else { 1.0 };
        let d = 5.0 * s * k;
        self.line(cx - d, cy - d, cx + d, cy + d, 1.6 * s, ink);
        self.line(cx - d, cy + d, cx + d, cy - d, 1.6 * s, ink);
    }

    /// Paint the home-page paper + wallpaper glows + dot grid into any area.
    /// `origin` positions the glows (a bar uses the stage, a lane uses itself),
    /// while the dot brush tiles from the window origin so separate fills join
    /// without a visible seam.
    unsafe fn draw_home_backdrop(&self, area: Rect, origin: Rect, light: bool) {
        self.fill(area, &self.sidebar_wallpaper);
        let glows = if light {
            &self.sidebar_glow_light
        } else {
            &self.sidebar_glow_dark
        };
        let width = origin.width().max(1.0);
        let height = origin.height().max(1.0);
        let geometry = [
            (0.06, -0.12, 0.90, 0.95),
            (1.04, 0.04, 0.80, 0.70),
            (0.64, 1.08, 0.70, 0.80),
        ];
        for (brush, (cx, cy, rx, ry)) in glows.iter().zip(geometry) {
            brush.SetCenter(Vector2::new(
                origin.left + width * cx,
                origin.top + height * cy,
            ));
            brush.SetRadiusX(width * rx);
            brush.SetRadiusY(height * ry);
            self.fill_brush(area, brush);
        }
        self.fill_brush(
            area,
            if light {
                &self.sidebar_dots_light
            } else {
                &self.sidebar_dots_dark
            },
        );
    }

    /// Home-page surface for the sidebar lane: warm paper, the same three
    /// wallpaper glows as the shell, and the faint dot texture. The lane is
    /// full height and flush with the picture; only the window's rounded edge
    /// clips its outer corners.
    unsafe fn draw_sidebar_wallpaper(&self, layout: &Layout, light: bool) {
        // Sample the same backdrop geometry as the letterbox bars. The lane is
        // flush with the stage, so the glows and dots continue across the seam
        // instead of starting a second wallpaper inside the sidebar.
        self.draw_home_backdrop(layout.sidebar, layout.stage, light);
    }

    unsafe fn draw_sidebar(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        self.draw_sidebar_wallpaper(layout, model.light_sidebar);
        // The selected tab sits on one shared ink pill that slides between
        // slots, mirroring the home-page bottom navigation.
        if let Some(index) = model.sidebar_tab.tab_index() {
            let tab = layout.sidebar_tabs[index];
            let target = tab.left + 4.0 * s;
            let width = (tab.width() - 8.0 * s).max(36.0 * s);
            if !self.tab_pill_ready {
                self.tab_pill_x = target;
                self.tab_pill_ready = true;
            }
            self.tab_pill_target = target;
            let diff = target - self.tab_pill_x;
            if diff.abs() <= 0.4 * s {
                self.tab_pill_x = target;
            } else {
                self.tab_pill_x += diff * 0.30;
            }
            let pill = Rect {
                left: self.tab_pill_x,
                top: tab.top + 4.0 * s,
                right: self.tab_pill_x + width,
                bottom: tab.bottom - 4.0 * s,
            };
            let rounded = D2D1_ROUNDED_RECT {
                rect: to_d2d(pill),
                radiusX: 9.0 * s,
                radiusY: 9.0 * s,
            };
            self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_text);
        }
        for (index, rect) in layout.sidebar_tabs.iter().enumerate() {
            let tab = SidebarTab::from_index(index);
            let active = model.sidebar_tab == tab
                || (model.sidebar_tab == SidebarTab::History && tab == SidebarTab::Comments);
            self.text_centered(
                tab.label(),
                *rect,
                if active {
                    &self.tab_active_format
                } else {
                    &self.tab_format
                },
                if active {
                    &self.sidebar_wallpaper
                } else {
                    &self.sidebar_text_dim
                },
            );
        }
        let clip = windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
            left: layout.sidebar.left,
            top: layout.sidebar.top + 52.0 * s,
            right: layout.sidebar.right,
            bottom: layout.sidebar.bottom,
        };
        self.d2d
            .PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        match model.sidebar_tab {
            SidebarTab::Info => self.draw_info(model, layout),
            SidebarTab::Episodes => self.draw_episodes(model, layout),
            SidebarTab::Comments => self.draw_comments(model, layout),
            SidebarTab::History => self.draw_history(model, layout),
            SidebarTab::Performance => self.draw_performance(model, layout),
            SidebarTab::Settings => self.draw_settings(model, layout),
        }
        self.d2d.PopAxisAlignedClip();
    }

    unsafe fn draw_info(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        let Some(info) = &model.info else {
            self.text_centered(
                "正在加载视频信息…",
                Rect {
                    left: content.left,
                    top: content.top + 80.0 * s,
                    right: content.right,
                    bottom: content.top + 104.0 * s,
                },
                &self.small_format,
                &self.sidebar_text_faint,
            );
            return;
        };
        // The hero keeps its proportions and the description gets the rest;
        // wheel scrolling moves the whole info column under the fixed tabs.
        let scroll = model.sidebar_scroll;
        let shifted = |rect: Rect| Rect {
            top: rect.top - scroll,
            bottom: rect.bottom - scroll,
            ..rect
        };

        // Full-width hero cover. It is the first thing the eye lands on; the
        // title follows on paper instead of being squeezed beside a stamp.
        let cover = shifted(layout.info_cover);
        let cover_drawn = info
            .cover
            .as_ref()
            .map(|image| self.draw_image_rounded(image, cover, 10.0 * s))
            .unwrap_or(false);
        if !cover_drawn {
            self.cover_placeholder(cover);
        }

        let title_rect = shifted(layout.info_title);
        let title = if info.title.trim().is_empty() {
            "未命名视频"
        } else {
            info.title.as_str()
        };
        self.text_wrapped(
            title,
            title_rect.left,
            title_rect.top,
            title_rect.width(),
            title_rect.height(),
            &self.title_format,
            if info.title.trim().is_empty() {
                &self.sidebar_text_faint
            } else {
                &self.sidebar_text
            },
        );

        // Uploader: a quiet user silhouette, the name in semibold, and a
        // small UP主 tag pinned to the right edge.
        let owner_rect = shifted(layout.info_owner);
        let owner = if info.owner.trim().is_empty() {
            "未知 UP 主"
        } else {
            info.owner.as_str()
        };
        let avatar_size = 24.0 * s;
        let avatar = Rect {
            left: owner_rect.left,
            top: owner_rect.top + (owner_rect.height() - avatar_size) * 0.5,
            right: owner_rect.left + avatar_size,
            bottom: owner_rect.top + (owner_rect.height() + avatar_size) * 0.5,
        };
        let avatar_center = Vector2::new(
            (avatar.left + avatar.right) * 0.5,
            (avatar.top + avatar.bottom) * 0.5,
        );
        self.d2d.FillEllipse(
            &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                point: avatar_center,
                radiusX: avatar_size * 0.5,
                radiusY: avatar_size * 0.5,
            },
            &self.sidebar_chip,
        );
        let head_center = Vector2::new(avatar_center.X, avatar_center.Y - 3.2 * s);
        self.d2d.FillEllipse(
            &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                point: head_center,
                radiusX: 3.0 * s,
                radiusY: 3.0 * s,
            },
            &self.accent_blue,
        );
        let shoulders = Rect {
            left: avatar_center.X - 5.5 * s,
            top: avatar_center.Y + 2.0 * s,
            right: avatar_center.X + 5.5 * s,
            bottom: avatar_center.Y + 9.5 * s,
        };
        let shoulders_round = D2D1_ROUNDED_RECT {
            rect: to_d2d(shoulders),
            radiusX: 5.0 * s,
            radiusY: 5.0 * s,
        };
        self.d2d
            .FillRoundedRectangle(&shoulders_round, &self.accent_blue);
        self.text_box(
            owner,
            Rect {
                left: avatar.right + 8.0 * s,
                top: owner_rect.top,
                right: owner_rect.right - 52.0 * s,
                bottom: owner_rect.bottom,
            },
            &self.comment_author_format,
            if info.owner.trim().is_empty() {
                &self.sidebar_text_faint
            } else {
                &self.sidebar_text
            },
            0,
        );
        self.text_box(
            "UP主",
            Rect {
                left: owner_rect.right - 48.0 * s,
                top: owner_rect.top,
                right: owner_rect.right,
                bottom: owner_rect.bottom,
            },
            &self.info_label_format,
            &self.sidebar_text_faint,
            2,
        );

        // Three equal metric columns. Labels are quiet; values are semibold.
        let stats = shifted(layout.info_stats);
        let column_w = stats.width() / 3.0;
        let date = format_date(info.pubdate);
        let columns = [
            ("播放", compact_number(info.views)),
            ("弹幕", compact_number(model.live_danmaku_count as u64)),
            ("日期", if date.is_empty() { "—".to_string() } else { date }),
        ];
        for (index, (label, value)) in columns.iter().enumerate() {
            let left = stats.left + index as f32 * column_w;
            let right = left + column_w - 8.0 * s;
            self.text_box(
                label,
                Rect {
                    left,
                    top: stats.top,
                    right,
                    bottom: stats.top + 16.0 * s,
                },
                &self.small_format,
                &self.sidebar_text_faint,
                0,
            );
            self.text_box(
                value,
                Rect {
                    left,
                    top: stats.top + 16.0 * s,
                    right,
                    bottom: stats.bottom,
                },
                &self.comment_author_format,
                &self.sidebar_text,
                0,
            );
        }

        // Video id field. The whole row is clickable; the icon on the right is
        // the explicit copy affordance.
        let id_rect = shifted(layout.info_id);
        let id_copy = shifted(layout.info_id_copy);
        let id_hot = model
            .pointer
            .map(|(x, y)| id_rect.contains(x, y))
            .unwrap_or(false);
        let id_round = D2D1_ROUNDED_RECT {
            rect: to_d2d(id_rect),
            radiusX: 10.0 * s,
            radiusY: 10.0 * s,
        };
        self.d2d.FillRoundedRectangle(
            &id_round,
            if id_hot {
                &self.sidebar_hover
            } else {
                &self.sidebar_chip
            },
        );
        self.text_box(
            "视频 ID",
            Rect {
                left: id_rect.left + 12.0 * s,
                top: id_rect.top,
                right: id_rect.left + 72.0 * s,
                bottom: id_rect.bottom,
            },
            &self.info_label_format,
            &self.sidebar_text_faint,
            0,
        );
        let has_id = !info.video_id.trim().is_empty();
        self.text_box(
            if has_id { info.video_id.as_str() } else { "—" },
            Rect {
                left: id_rect.left + 76.0 * s,
                top: id_rect.top,
                right: id_copy.left - 6.0 * s,
                bottom: id_rect.bottom,
            },
            &self.info_id_format,
            if has_id {
                &self.sidebar_text
            } else {
                &self.sidebar_text_faint
            },
            0,
        );
        let copy_hot = model
            .pointer
            .map(|(x, y)| id_copy.contains(x, y))
            .unwrap_or(false);
        if copy_hot {
            let copy_round = D2D1_ROUNDED_RECT {
                rect: to_d2d(id_copy),
                radiusX: 8.0 * s,
                radiusY: 8.0 * s,
            };
            self.d2d
                .FillRoundedRectangle(&copy_round, &self.sidebar_hover);
        }
        self.draw_copy_icon(
            id_copy,
            s,
            if copy_hot {
                &self.accent_blue
            } else {
                &self.sidebar_text_dim
            },
        );

        // 互动: one hairline, a section label, then three equal actions.
        let divider_y = id_rect.bottom + 14.0 * s;
        self.line(
            content.left,
            divider_y,
            content.right,
            divider_y,
            1.0 * s,
            &self.sidebar_chip,
        );
        self.text_box(
            "互动",
            Rect {
                left: content.left,
                top: divider_y + 10.0 * s,
                right: content.right,
                bottom: divider_y + 28.0 * s,
            },
            &self.info_label_format,
            &self.sidebar_text_dim,
            0,
        );
        let labels = [
            if model.liked { "已点赞" } else { "点赞" },
            if model.coined { "已投币" } else { "投币" },
            if model.favorited {
                "已收藏"
            } else {
                "收藏"
            },
        ];
        let active = [model.liked, model.coined, model.favorited];
        for (index, rect) in layout.info_actions.iter().enumerate() {
            let rect = shifted(*rect);
            let hot = model
                .pointer
                .map(|(x, y)| rect.contains(x, y))
                .unwrap_or(false);
            let brush = if active[index] {
                &self.accent_blue
            } else if hot {
                &self.sidebar_hover
            } else {
                &self.sidebar_chip
            };
            let rounded = D2D1_ROUNDED_RECT {
                rect: to_d2d(rect),
                radiusX: 10.0 * s,
                radiusY: 10.0 * s,
            };
            self.d2d.FillRoundedRectangle(&rounded, brush);
            self.text_centered(
                labels[index],
                rect,
                &self.tab_active_format,
                if active[index] {
                    &self.accent_fg
                } else {
                    &self.sidebar_text
                },
            );
        }

        // 简介: another hairline and a body with a deliberately open line box.
        let actions_bottom = shifted(layout.info_actions[0]).bottom;
        let desc_divider_y = actions_bottom + 12.0 * s;
        self.line(
            content.left,
            desc_divider_y,
            content.right,
            desc_divider_y,
            1.0 * s,
            &self.sidebar_chip,
        );
        self.text_box(
            "简介",
            Rect {
                left: content.left,
                top: desc_divider_y + 8.0 * s,
                right: content.right,
                bottom: desc_divider_y + 26.0 * s,
            },
            &self.info_label_format,
            &self.sidebar_text_dim,
            0,
        );
        let description = if info.description.trim().is_empty() {
            "暂无简介"
        } else {
            info.description.trim()
        };
        // The description mask is measured against its full text height, not
        // the changing viewport. The box moves with the scroll and the lane
        // clip cuts it; a per-frame changing height would re-rasterise every
        // line and make the sidebar stutter.
        let desc_top = layout.info_description.top - scroll;
        let desc_height = self
            .info_description_height(description, layout.info_description.width())
            .max(8.0 * s);
        if desc_height > 8.0 * s {
            self.text_wrapped(
                description,
                layout.info_description.left,
                desc_top,
                layout.info_description.width(),
                desc_height,
                &self.info_body_format,
                if info.description.trim().is_empty() {
                    &self.sidebar_text_faint
                } else {
                    &self.sidebar_text_dim
                },
            );
        }
    }

    unsafe fn cover_placeholder(&self, rect: Rect) {
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 10.0 * self.layout.scale,
            radiusY: 10.0 * self.layout.scale,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_chip);
        self.text_centered(
            "暂无封面",
            rect,
            &self.small_format,
            &self.sidebar_text_faint,
        );
    }

    unsafe fn draw_episodes(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        let pages = if model.pages.is_empty() {
            model
                .info
                .as_ref()
                .map(|info| info.pages.as_slice())
                .unwrap_or(&[])
        } else {
            model.pages.as_slice()
        };
        if pages.is_empty() {
            self.text(
                "暂无分P信息",
                content.left + 2.0 * s,
                content.top + 66.0 * s,
                content.width(),
                &self.comment_body_format,
                &self.sidebar_text_dim,
            );
            self.draw_sidebar_section_header("选集", "0 个分P", layout, model.sidebar_scroll);
            return;
        }
        let current = if model.current_page < pages.len() {
            model.current_page
        } else {
            model
                .info
                .as_ref()
                .map(|info| info.current_page)
                .unwrap_or(0)
        };
        let content_h = pages.len() as f32 * 52.0 * s;
        let visible_h = (content.height() - 60.0 * s).max(1.0);
        let max_scroll = (content_h - visible_h).max(0.0);
        let scroll = model.sidebar_scroll.clamp(0.0, max_scroll);
        for (index, page) in pages.iter().enumerate() {
            let rect = layout.episode_row(index, scroll);
            if rect.bottom < content.top || rect.top > content.bottom {
                continue;
            }
            if rect.top > content.bottom - 8.0 * s {
                continue;
            }
            let active = index == current;
            let hot = model
                .pointer
                .map(|(x, y)| rect.contains(x, y))
                .unwrap_or(false);
            if active || hot {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(rect),
                    radiusX: 10.0 * s,
                    radiusY: 10.0 * s,
                };
                self.d2d.FillRoundedRectangle(
                    &rounded,
                    if active {
                        &self.sidebar_chip
                    } else {
                        &self.sidebar_hover
                    },
                );
            }
            if active {
                self.fill(
                    Rect {
                        left: rect.left + 1.0 * s,
                        top: rect.top + 12.0 * s,
                        right: rect.left + 3.0 * s,
                        bottom: rect.bottom - 12.0 * s,
                    },
                    &self.accent_blue,
                );
            }
            self.text_box(
                &format!("P{}", index + 1),
                Rect {
                    left: rect.left + 12.0 * s,
                    right: rect.left + 48.0 * s,
                    ..rect
                },
                &self.comment_meta_format,
                if active {
                    &self.accent_blue
                } else {
                    &self.sidebar_text_faint
                },
                0,
            );
            self.text_box(
                &ellipsize(&page.title, 20),
                Rect {
                    left: rect.left + 48.0 * s,
                    right: (rect.right - 72.0 * s).max(rect.left + 108.0 * s),
                    ..rect
                },
                &self.comment_body_format,
                if active {
                    &self.sidebar_text
                } else {
                    &self.sidebar_text_dim
                },
                0,
            );
            self.text_box(
                &format_time(page.duration.saturating_mul(1000)),
                Rect {
                    left: rect.right - 68.0 * s,
                    right: rect.right - 10.0 * s,
                    ..rect
                },
                &self.comment_meta_format,
                &self.sidebar_text_faint,
                2,
            );
        }
        self.draw_sidebar_section_header(
            "选集",
            &format!("{} 个分P", pages.len()),
            layout,
            model.sidebar_scroll,
        );
    }

    /// Section title/meta at the top of a scrolling tab. It is part of the
    /// scroll content, sits directly on the home wallpaper, and draws no band
    /// or divider line. Rows begin after its 52px slot.
    unsafe fn draw_sidebar_section_header(
        &self,
        title: &str,
        meta: &str,
        layout: &Layout,
        scroll: f32,
    ) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        let top = content.top - scroll;
        // The header owns the first 52px slot, the same slot the row list
        // starts after. Both labels sit on one centred line box instead of
        // being pinned to the slot's top edge.
        let slot = Rect {
            left: content.left,
            top,
            right: content.right,
            bottom: top + 52.0 * s,
        };
        self.text_box(
            title,
            Rect {
                right: slot.left + 120.0 * s,
                ..slot
            },
            &self.comment_name_format,
            &self.sidebar_text,
            0,
        );
        let meta_left = content.left + title.chars().count() as f32 * 13.0 * s + 8.0 * s;
        self.text_box(
            meta,
            Rect {
                left: meta_left,
                right: slot.right,
                ..slot
            },
            &self.comment_meta_format,
            &self.sidebar_text_faint,
            0,
        );
    }

    /// Wrap one comment/reply body with the exact GDI face and width the row
    /// will be drawn with. The measured row height and the painted lines then
    /// come from the same list, so nothing clips or leaves a white gap.
    fn wrap_with_format(&self, text: &str, width: f32, format: &IDWriteTextFormat) -> Vec<String> {
        if width <= 2.0 {
            return vec![text.to_string()];
        }
        if let Some(style) = self
            .styles
            .get(&(windows::core::Interface::as_raw(format) as usize))
        {
            if let Some(gdi) = self.gdi.borrow_mut().as_mut() {
                let lines = unsafe { gdi.wrap(text, *style, width - 2.0) };
                if !lines.is_empty() {
                    return lines;
                }
            }
            return wrap_text_estimate(text, width, style.size);
        }
        wrap_text_estimate(text, width, 14.0 * self.layout.scale)
    }

    /// Measure every comment row once per generation. Heights are relative to
    /// the top of the scrolling list; `draw_comments` subtracts the scroll.
    fn ensure_comment_layout(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale.max(0.75);
        let content = layout.sidebar_content;
        let key = (
            model.comments_generation,
            content.width().to_bits(),
            content.height().to_bits(),
            s.to_bits(),
            u32::from(model.comments_loading),
        );
        if self.comment_layout_key == Some(key) {
            return;
        }
        let text_left = 46.0 * s;
        let text_width = (content.width() - text_left - 2.0 * s).max(40.0);
        let pad_top = 14.0 * s;
        let header_h = 42.0 * s;
        let body_line = 21.0 * s;
        let reply_indent = 12.0 * s;
        let reply_gap = 6.0 * s;
        let reply_avatar_gap = 28.0 * s;
        let mut rows = Vec::with_capacity(model.comments.len());
        let mut y = 0.0;
        for comment in &model.comments {
            let body_lines = self.wrap_with_format(
                &comment.message,
                text_width - 2.0 * s,
                &self.comment_text_format,
            );
            let body_h = body_lines.len().max(1) as f32 * body_line;
            let mut row = CommentRowLayout {
                rpid: comment.rpid,
                top: y,
                height: 0.0,
                body_lines,
                replies: Vec::new(),
                links: Vec::new(),
            };
            // Collapsed rows show no replies at all; expanding reveals the
            // thread. This keeps the top-level list compact and makes the
            // 收起 action truly collapse.
            let preview_len = if comment.expanded {
                comment.replies.len()
            } else {
                0
            };
            let mut reply_y = pad_top + header_h + body_h + 10.0 * s;
            for (index, reply) in comment.replies.iter().take(preview_len).enumerate() {
                let reply_width =
                    (text_width - reply_indent - reply_avatar_gap - 2.0 * s).max(60.0);
                let lines =
                    self.wrap_with_format(&reply.message, reply_width, &self.reply_text_format);
                let height = 20.0 * s + lines.len().max(1) as f32 * 18.5 * s + 8.0 * s;
                row.replies.push(ReplyRowLayout {
                    reply_index: index,
                    top: reply_y,
                    height,
                    lines,
                });
                reply_y += height + reply_gap;
            }
            if comment.replies_loading && comment.replies.is_empty() {
                reply_y += 20.0 * s;
            }
            let total_replies = comment.reply_count.max(comment.replies.len() as i64);
            if comment.expanded || total_replies > 0 {
                let link_h = 24.0 * s;
                row.links.push(CommentLinkLayout {
                    rect: Rect {
                        left: content.left + text_left + reply_indent,
                        top: y + reply_y,
                        right: content.right,
                        bottom: y + reply_y + link_h,
                    },
                    action: CommentAction::Toggle,
                });
                reply_y += link_h;
            }
            if comment.expanded && !comment.replies_end {
                let link_h = 24.0 * s;
                row.links.push(CommentLinkLayout {
                    rect: Rect {
                        left: content.left + text_left + reply_indent,
                        top: y + reply_y,
                        right: content.right,
                        bottom: y + reply_y + link_h,
                    },
                    action: CommentAction::More,
                });
                reply_y += link_h;
            }
            row.height = reply_y + 14.0 * s;
            y += row.height;
            rows.push(row);
        }
        self.comment_rows = rows;
        self.comment_list_height = y;
        self.comment_layout_key = Some(key);
    }

    /// Height of the measured comment list, used by the engine's scroll clamp
    /// and the near-bottom auto-fetch trigger.
    pub fn comments_list_height(&self) -> f32 {
        self.comment_list_height
    }

    /// Height of the info tab's description at `width`, using the same airy
    /// line box as the draw pass. The engine adds it to the fixed hero height
    /// to know how far the info tab can scroll.
    pub fn info_description_height(&self, text: &str, width: f32) -> f32 {
        if text.trim().is_empty() || width <= 1.0 {
            return 0.0;
        }
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            if let Ok(layout) = self.dwrite.CreateTextLayout(
                &utf16,
                &self.info_body_format,
                width,
                100_000.0,
            ) {
                let mut metrics = DWRITE_TEXT_METRICS::default();
                if layout.GetMetrics(&mut metrics).is_ok() {
                    return metrics.height;
                }
            }
        }
        let line_height = self
            .styles
            .get(&(windows::core::Interface::as_raw(&self.info_body_format) as usize))
            .map(|style| style.line_height)
            .unwrap_or(13.5 * self.layout.scale * 1.55);
        let per_line = (width / (13.5 * self.layout.scale)).floor().max(1.0);
        let lines = (text.chars().count() as f32 / per_line).ceil().max(1.0);
        lines * line_height
    }

    /// True when the measured list belongs to the current comment generation.
    /// The engine waits for this before trusting the auto-fetch threshold, so
    /// a page that just arrived cannot immediately trigger another fetch
    /// against a stale/empty height.
    pub fn comments_layout_ready(&self, generation: u64) -> bool {
        self.comment_layout_key
            .map(|key| key.0 == generation)
            .unwrap_or(false)
    }

    /// Hit-test a thread link. The row cache stores unscrolled rectangles, so
    /// the caller passes the current smooth scroll offset.
    pub fn comment_hit(&self, x: f32, y: f32, scroll: f32) -> Option<CommentHit> {
        let content = self.layout.sidebar_content;
        if !content.contains(x, y) {
            return None;
        }
        let list_top = self.layout.comments_header.bottom;
        for row in &self.comment_rows {
            let row_top = list_top + row.top - scroll;
            if y < row_top || y > row_top + row.height {
                continue;
            }
            for link in &row.links {
                let rect = Rect {
                    top: list_top + link.rect.top - scroll,
                    bottom: list_top + link.rect.bottom - scroll,
                    ..link.rect
                };
                if rect.contains(x, y) {
                    return Some(CommentHit {
                        rpid: row.rpid,
                        action: link.action,
                    });
                }
            }
        }
        None
    }

    /// Comments header: title/count on the left and a two-segment sort
    /// control on the right with a gliding ink pill. It is the first block of
    /// the comment flow, so it scrolls away with the rows.
    unsafe fn draw_comments_header(&self, model: &UiModel, layout: &Layout, scroll: f32) {
        let s = layout.scale;
        let shift = |rect: Rect| Rect {
            top: rect.top - scroll,
            bottom: rect.bottom - scroll,
            ..rect
        };
        let header = shift(layout.comments_header);
        let sort0 = shift(layout.comments_sort[0]);
        let sort1 = shift(layout.comments_sort[1]);
        let bg = Rect {
            left: sort0.left,
            top: sort0.top,
            right: sort1.right,
            bottom: sort0.bottom,
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(bg),
            radiusX: 9.0 * s,
            radiusY: 9.0 * s,
        };
        self.d2d
            .FillRoundedRectangle(&rounded, &self.sidebar_panel_bg);

        let mix = model.comments_sort_mix.clamp(0.0, 1.0);
        let c0 = (sort0.left + sort0.right) * 0.5;
        let c1 = (sort1.left + sort1.right) * 0.5;
        let cx = c0 + (c1 - c0) * mix;
        let pill_w = (sort0.width() - 10.0 * s).max(36.0 * s);
        let pill = Rect {
            left: cx - pill_w * 0.5,
            top: sort0.top + 2.0 * s,
            right: cx + pill_w * 0.5,
            bottom: sort0.bottom - 2.0 * s,
        };
        let pill_round = D2D1_ROUNDED_RECT {
            rect: to_d2d(pill),
            radiusX: 8.0 * s,
            radiusY: 8.0 * s,
        };
        self.d2d
            .FillRoundedRectangle(&pill_round, &self.sidebar_text);

        let active = if mix < 0.5 { 0 } else { 1 };
        for (index, label) in ["最热", "最新"].iter().enumerate() {
            self.text_centered(
                label,
                if index == 0 { sort0 } else { sort1 },
                if index == active {
                    &self.tab_active_format
                } else {
                    &self.tab_format
                },
                if index == active {
                    &self.sidebar_wallpaper
                } else {
                    &self.sidebar_text_dim
                },
            );
        }

        self.text_box(
            "评论",
            Rect {
                right: header.left + 38.0 * s,
                ..header
            },
            &self.comment_author_format,
            &self.sidebar_text,
            0,
        );
        self.text_box(
            &format!("共 {} 条", model.comments_total.max(0)),
            Rect {
                left: header.left + 42.0 * s,
                right: bg.left - 8.0 * s,
                ..header
            },
            &self.small_format,
            &self.sidebar_text_faint,
            0,
        );
    }

    /// Comments tab: scrolling header + measured rows, inline 楼中楼
    /// previews, a 2px thread rail and a sliding sort pill. Every page arrives
    /// automatically near the bottom; there is no load-more button.
    unsafe fn draw_comments(&mut self, model: &UiModel, layout: &Layout) {
        self.ensure_comment_layout(model, layout);
        let s = layout.scale;
        let content = layout.sidebar_content;
        let list_top = layout.comments_header.bottom;
        let viewport = (content.bottom - list_top).max(1.0);
        let scroll = model
            .sidebar_scroll
            .clamp(0.0, (self.comment_list_height - viewport).max(0.0));
        self.draw_comments_header(model, layout, scroll);

        if model.comments.is_empty() {
            let text = if model.comments_loading {
                "正在加载评论…"
            } else {
                "还没有评论"
            };
            self.text_centered(
                text,
                Rect {
                    left: content.left,
                    top: list_top + 20.0 * s,
                    right: content.right,
                    bottom: list_top + 60.0 * s,
                },
                &self.comment_meta_format,
                &self.sidebar_text_dim,
            );
            return;
        }

        let enter = model.comments_enter.clamp(0.0, 1.0);
        let offset = (1.0 - enter) * 10.0 * s;
        let layer = if enter < 0.995 {
            unsafe { self.d2d.CreateLayer(None).ok() }
        } else {
            None
        };
        if let Some(layer) = &layer {
            let params = windows::Win32::Graphics::Direct2D::D2D1_LAYER_PARAMETERS1 {
                contentBounds: to_d2d(Rect {
                    left: content.left,
                    top: content.top,
                    right: content.right,
                    bottom: content.bottom,
                }),
                geometricMask: std::mem::ManuallyDrop::new(None),
                maskAntialiasMode:
                    windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: Matrix3x2::identity(),
                opacity: enter,
                opacityBrush: std::mem::ManuallyDrop::new(None),
                layerOptions: windows::Win32::Graphics::Direct2D::D2D1_LAYER_OPTIONS1_NONE,
            };
            unsafe { self.d2d.PushLayer(&params, layer) };
        }

        let text_left = content.left + 46.0 * s;
        let text_width = (content.width() - 46.0 * s - 2.0 * s).max(40.0);
        let avatar = 36.0 * s;
        // The row cache is borrowed for the whole loop, while drawing avatars
        // needs `&mut self`; take it out and put it back after painting.
        let rows = std::mem::take(&mut self.comment_rows);
        for (row, comment) in rows.iter().zip(model.comments.iter()) {
            let row_top = list_top + row.top - scroll + offset;
            let row_bottom = row_top + row.height;
            if row_bottom < content.top || row_top > content.bottom {
                continue;
            }
            let pad_top = 14.0 * s;
            let body_top = row_top + pad_top + 42.0 * s;
            let body_bottom = body_top + row.body_lines.len().max(1) as f32 * 21.0 * s;
            let hover_top = row_top + 4.0 * s;
            let hover_bottom = body_bottom + 8.0 * s;
            let hot = model
                .pointer
                .map(|(x, y)| {
                    x >= content.left && x <= content.right && y >= hover_top && y <= hover_bottom
                })
                .unwrap_or(false);
            if hot {
                let hover = D2D1_ROUNDED_RECT {
                    rect: to_d2d(Rect {
                        left: content.left - 6.0 * s,
                        top: hover_top,
                        right: content.right + 2.0 * s,
                        bottom: hover_bottom,
                    }),
                    radiusX: 10.0 * s,
                    radiusY: 10.0 * s,
                };
                self.d2d.FillRoundedRectangle(&hover, &self.sidebar_hover);
            }

            let avatar_rect = Rect {
                left: content.left,
                top: row_top + pad_top,
                right: content.left + avatar,
                bottom: row_top + pad_top + avatar,
            };
            let avatar_drawn = comment
                .avatar
                .as_ref()
                .map(|image| self.draw_image_circular(image, avatar_rect))
                .unwrap_or(false);
            if !avatar_drawn {
                self.d2d.FillEllipse(
                    &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                        point: Vector2::new(
                            (avatar_rect.left + avatar_rect.right) * 0.5,
                            (avatar_rect.top + avatar_rect.bottom) * 0.5,
                        ),
                        radiusX: avatar * 0.5,
                        radiusY: avatar * 0.5,
                    },
                    &self.sidebar_avatar_bg,
                );
                let initial: String = comment.user.chars().take(1).collect();
                self.text_centered(
                    &initial,
                    avatar_rect,
                    &self.comment_name_format,
                    &self.sidebar_text,
                );
            }

            self.text_box(
                &ellipsize(&comment.user, 22),
                Rect {
                    left: text_left,
                    top: row_top + pad_top,
                    right: text_left + text_width - 64.0 * s,
                    bottom: row_top + pad_top + 18.0 * s,
                },
                &self.comment_author_format,
                &self.sidebar_text,
                0,
            );
            let time = format_comment_time(comment.ctime);
            let time_label = if comment.pinned {
                format!("置顶 · {time}")
            } else {
                time
            };
            self.text_box(
                &time_label,
                Rect {
                    left: text_left,
                    top: row_top + pad_top + 18.0 * s,
                    right: text_left + text_width - 64.0 * s,
                    bottom: row_top + pad_top + 38.0 * s,
                },
                &self.small_format,
                if comment.pinned {
                    &self.accent_blue
                } else {
                    &self.sidebar_text_faint
                },
                0,
            );
            self.text_box(
                &format!("赞 {}", compact_number(comment.likes)),
                Rect {
                    left: content.right - 62.0 * s,
                    top: row_top + pad_top,
                    right: content.right,
                    bottom: row_top + pad_top + 20.0 * s,
                },
                &self.small_format,
                &self.sidebar_text_faint,
                2,
            );

            for (line_index, line) in row.body_lines.iter().enumerate() {
                self.text(
                    line,
                    text_left,
                    body_top + line_index as f32 * 21.0 * s,
                    text_width,
                    &self.comment_text_format,
                    &self.sidebar_text,
                );
            }

            if !row.replies.is_empty() {
                let last = row.replies.last().map(|r| r.top + r.height).unwrap_or(0.0);
                self.fill(
                    Rect {
                        left: text_left,
                        top: body_bottom + 6.0 * s,
                        right: text_left + 2.0 * s,
                        bottom: row_top + last - 2.0 * s,
                    },
                    &self.sidebar_line,
                );
            }
            for reply_row in &row.replies {
                let Some(reply) = comment.replies.get(reply_row.reply_index) else {
                    continue;
                };
                let reply_rect = Rect {
                    left: text_left + 12.0 * s,
                    top: row_top + reply_row.top,
                    right: content.right,
                    bottom: row_top + reply_row.top + reply_row.height,
                };
                let mini = Rect {
                    left: reply_rect.left,
                    top: reply_rect.top + 1.0 * s,
                    right: reply_rect.left + 20.0 * s,
                    bottom: reply_rect.top + 21.0 * s,
                };
                let drawn = reply
                    .avatar
                    .as_ref()
                    .map(|image| self.draw_image_circular(image, mini))
                    .unwrap_or(false);
                if !drawn {
                    self.d2d.FillEllipse(
                        &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                            point: Vector2::new(
                                (mini.left + mini.right) * 0.5,
                                (mini.top + mini.bottom) * 0.5,
                            ),
                            radiusX: mini.width() * 0.5,
                            radiusY: mini.height() * 0.5,
                        },
                        &self.sidebar_avatar_bg,
                    );
                    let initial: String = reply.user.chars().take(1).collect();
                    self.text_centered(&initial, mini, &self.hint_format, &self.sidebar_text);
                }
                self.text_box(
                    &ellipsize(&reply.user, 18),
                    Rect {
                        left: reply_rect.left + 28.0 * s,
                        top: reply_rect.top + 1.0 * s,
                        right: reply_rect.right - 104.0 * s,
                        bottom: reply_rect.top + 17.0 * s,
                    },
                    &self.reply_author_format,
                    &self.sidebar_text,
                    0,
                );
                self.text_box(
                    &format!(
                        "赞 {} · {}",
                        compact_number(reply.likes),
                        format_comment_time(reply.ctime)
                    ),
                    Rect {
                        left: reply_rect.right - 104.0 * s,
                        top: reply_rect.top + 1.0 * s,
                        right: reply_rect.right,
                        bottom: reply_rect.top + 17.0 * s,
                    },
                    &self.small_format,
                    &self.sidebar_text_faint,
                    2,
                );
                for (line_index, line) in reply_row.lines.iter().enumerate() {
                    self.text(
                        line,
                        reply_rect.left + 28.0 * s,
                        reply_rect.top + 20.0 * s + line_index as f32 * 18.5 * s,
                        (reply_rect.right - reply_rect.left - 30.0 * s).max(60.0),
                        &self.reply_text_format,
                        &self.sidebar_text_dim,
                    );
                }
            }
            if comment.replies_loading && comment.replies.is_empty() {
                self.text_box(
                    "正在加载回复…",
                    Rect {
                        left: text_left + 12.0 * s,
                        top: body_bottom + 10.0 * s,
                        right: content.right,
                        bottom: body_bottom + 30.0 * s,
                    },
                    &self.comment_meta_format,
                    &self.sidebar_text_faint,
                    0,
                );
            }

            for link in &row.links {
                let link_top = list_top + link.rect.top - scroll + offset;
                let link_rect = Rect {
                    left: link.rect.left,
                    top: link_top,
                    right: link.rect.right,
                    bottom: link_top + link.rect.height(),
                };
                let label = match link.action {
                    CommentAction::Toggle => {
                        if comment.expanded {
                            "收起回复".to_string()
                        } else {
                            format!(
                                "查看全部 {} 条回复",
                                comment.reply_count.max(comment.replies.len() as i64)
                            )
                        }
                    }
                    CommentAction::More => {
                        if comment.replies_loading {
                            "正在加载回复…".to_string()
                        } else {
                            "加载更多回复".to_string()
                        }
                    }
                };
                self.text_box(
                    &label,
                    link_rect,
                    &self.tab_active_format,
                    if comment.replies_loading && link.action == CommentAction::More {
                        &self.sidebar_text_faint
                    } else {
                        &self.accent_blue
                    },
                    0,
                );
            }

            self.line(
                text_left,
                row_bottom - 8.0 * s,
                content.right,
                row_bottom - 8.0 * s,
                1.0 * s,
                &self.sidebar_line,
            );
        }
        self.comment_rows = rows;

        if layer.is_some() {
            unsafe { self.d2d.PopLayer() };
        }
    }

    unsafe fn draw_history(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        // Header row is the 30px slot above 实时弹幕; centre its two labels in
        // that slot instead of leaving them top-aligned.
        let head = Rect {
            left: content.left,
            top: content.top,
            right: content.right,
            bottom: content.top + 30.0 * s,
        };
        self.text_box(
            "弹幕历史",
            Rect {
                right: head.left + 120.0 * s,
                ..head
            },
            &self.comment_body_format,
            &self.sidebar_text,
            0,
        );
        self.text_box(
            &format!("实时 {} 条", model.live_danmaku_count),
            Rect {
                left: content.left + 60.0 * s,
                right: head.right,
                ..head
            },
            &self.comment_meta_format,
            &self.sidebar_text_faint,
            0,
        );
        let live = layout.history_live;
        let live_active = model.history_date.is_none();
        let hot = model
            .pointer
            .map(|(x, y)| live.contains(x, y))
            .unwrap_or(false);
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(live),
            radiusX: 7.0 * s,
            radiusY: 7.0 * s,
        };
        self.d2d.FillRoundedRectangle(
            &rounded,
            if live_active {
                &self.sidebar_chip
            } else if hot {
                &self.sidebar_hover
            } else {
                &self.sidebar_panel_bg
            },
        );
        self.text_box(
            "实时弹幕",
            Rect {
                left: live.left + 10.0 * s,
                top: live.top,
                right: live.right - 10.0 * s,
                bottom: live.bottom,
            },
            &self.comment_meta_format,
            if live_active {
                &self.sidebar_text
            } else {
                &self.sidebar_text_dim
            },
            0,
        );

        if model.history_dates_loading {
            self.text(
                "正在获取历史弹幕日期…",
                content.left,
                live.bottom + 14.0 * s,
                content.width(),
                &self.comment_meta_format,
                &self.sidebar_text_dim,
            );
            return;
        }
        if let Some(error) = &model.history_dates_error {
            self.text_wrapped(
                &format!("历史日期加载失败：{}", ellipsize(error, 80)),
                content.left,
                live.bottom + 12.0 * s,
                content.width(),
                54.0 * s,
                &self.wrap_comment,
                &self.accent,
            );
            return;
        }
        if model.history_dates.is_empty() {
            self.text(
                "暂无历史弹幕日期",
                content.left,
                live.bottom + 14.0 * s,
                content.width(),
                &self.comment_meta_format,
                &self.sidebar_text_dim,
            );
            return;
        }
        if model.history_loading {
            self.text(
                "正在加载历史弹幕…",
                content.left,
                live.bottom + 12.0 * s,
                content.width(),
                &self.comment_meta_format,
                &self.accent,
            );
        } else if let Some(error) = &model.history_error {
            self.text_wrapped(
                &format!("历史弹幕加载失败：{}", ellipsize(error, 76)),
                content.left,
                live.bottom + 12.0 * s,
                content.width(),
                46.0 * s,
                &self.wrap_comment,
                &self.accent,
            );
        }
        for (index, date) in model.history_dates.iter().enumerate() {
            let rect = layout.history_row(index, model.sidebar_scroll);
            if rect.bottom < layout.sidebar_content.top || rect.top > layout.sidebar_content.bottom
            {
                continue;
            }
            let active = model.history_date.as_deref() == Some(date.as_str());
            let hot = model
                .pointer
                .map(|(x, y)| rect.contains(x, y))
                .unwrap_or(false);
            if active || hot {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(rect),
                    radiusX: 6.0 * s,
                    radiusY: 6.0 * s,
                };
                self.d2d.FillRoundedRectangle(
                    &rounded,
                    if active {
                        &self.sidebar_chip
                    } else {
                        &self.sidebar_hover
                    },
                );
            }
            self.text_box(
                date,
                Rect {
                    left: rect.left + 10.0 * s,
                    right: rect.right - 10.0 * s,
                    ..rect
                },
                &self.comment_meta_format,
                if active {
                    &self.sidebar_text
                } else {
                    &self.sidebar_text_dim
                },
                0,
            );
            if active {
                self.text_box(
                    &format!("{} 条", model.history_count),
                    Rect {
                        left: rect.right - 78.0 * s,
                        right: rect.right - 8.0 * s,
                        ..rect
                    },
                    &self.comment_meta_format,
                    &self.accent,
                    2,
                );
            }
        }
        let footer = if model.history_date.is_some() {
            "正在显示历史弹幕 · 关闭面板恢复实时"
        } else {
            "点击日期查看当天弹幕"
        };
        self.text(
            footer,
            content.left,
            content.bottom - 20.0 * s,
            content.width(),
            &self.comment_meta_format,
            &self.sidebar_text_faint,
        );
    }

    unsafe fn draw_performance(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        let content_h = PERFORMANCE_CONTENT_H * s;
        let visible_h = content.height().max(1.0);
        let max_scroll = (content_h - visible_h).max(0.0);
        let scroll = model.sidebar_scroll.clamp(0.0, max_scroll);
        let mut y = content.top + PERF_TOP_PAD * s - scroll;
        let visible = |y: f32| y >= content.top - 18.0 * s && y + 18.0 * s <= content.bottom;
        // 24 × 6 = 144 fixed timeline slices. Every cell is exactly 1/144 of
        // the video, not a bucket sized from the remaining cache.
        let cols = 24usize;
        let grid_rows = 6usize;
        let cells = cols * grid_rows;
        // The network block keeps the sidebar's standard label level. There is
        // no inner 性能 heading: the tab strip already labels this panel.
        let section = |name: &str, y: &mut f32| {
            *y += PERF_SECTION_GAP * s * 0.5;
            if visible(*y) {
                self.line(
                    content.left,
                    *y,
                    content.right,
                    *y,
                    1.0 * s,
                    &self.sidebar_chip,
                );
            }
            *y += PERF_SECTION_GAP * s * 0.5;
            if visible(*y) {
                self.text_box(
                    name,
                    Rect {
                        left: content.left,
                        top: *y,
                        right: content.right,
                        bottom: *y + PERF_SECTION_H * s,
                    },
                    &self.info_label_format,
                    &self.sidebar_text_dim,
                    0,
                );
            }
            *y += PERF_SECTION_H * s;
        };
        let row = |label: &str, value: &str, y: &mut f32| {
            if visible(*y) {
                self.text_box(
                    label,
                    Rect {
                        left: content.left,
                        top: *y,
                        right: content.left + content.width() * 0.44,
                        bottom: *y + 24.0 * s,
                    },
                    &self.comment_meta_format,
                    &self.sidebar_text_dim,
                    0,
                );
                self.text_box(
                    value,
                    Rect {
                        left: content.right - content.width() * 0.58,
                        top: *y,
                        right: content.right,
                        bottom: *y + 24.0 * s,
                    },
                    &self.comment_body_format,
                    &self.sidebar_text,
                    2,
                );
            }
            *y += 24.0 * s;
        };

        // 性能: the only control the tab needs. The tab label already says
        // 性能, so there is no inner title or FPS line here — just the switch.
        let switch_row = layout.performance_hud_row(scroll);
        if visible(switch_row.top) {
            self.text_box(
                "右上角性能显示",
                Rect {
                    left: content.left,
                    top: switch_row.top,
                    right: switch_row.right - 66.0 * s,
                    bottom: switch_row.bottom,
                },
                &self.comment_body_format,
                &self.sidebar_text,
                0,
            );
            let hot = model
                .pointer
                .map(|(x, y)| switch_row.contains(x, y))
                .unwrap_or(false);
            self.sidebar_switch(
                layout.performance_hud_switch(scroll),
                model.stats,
                hot,
                s,
            );
        }
        y = switch_row.bottom;

        // 网络: the live transfer data plus where playback is and how much of
        // the file has arrived. No duplicated FPS/frame counters here.
        section("网络", &mut y);
        if visible(y - PERF_SECTION_H * s) {
            self.text_right(
                &format!("{cells} 格 · 每格 {:.1}%", 100.0 / cells as f32),
                content.right,
                y - PERF_SECTION_H * s,
                content.width() * 0.6,
                &self.small_format,
                &self.sidebar_text_faint,
            );
        }
        let position = model.position_ms.min(model.duration_ms.max(1));
        let played_percent = if model.duration_ms > 0 {
            (position as f32 / model.duration_ms as f32 * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        row(
            "播放位置",
            &format!(
                "{} / {}",
                crate::format::format_time_ms(position),
                crate::format::format_time_ms(model.duration_ms)
            ),
            &mut y,
        );
        row("已播放", &format!("{:.0}%", played_percent), &mut y);
        let loaded_value = if model.total_bytes > 0 {
            let percent =
                (model.loaded_bytes as f64 / model.total_bytes as f64 * 100.0).clamp(0.0, 100.0);
            format!(
                "{:.0}% · {} / {}",
                percent,
                format_bytes(model.loaded_bytes as f64),
                format_bytes(model.total_bytes as f64)
            )
        } else if model.source_remote {
            "边下边播（无缓存）".to_string()
        } else {
            "100% · 本地文件".to_string()
        };
        row("已加载", &loaded_value, &mut y);
        let lead_s = model
            .net_lead_s
            .max((model.queue_len as f64 + 1.0) / model.source_fps.max(24.0))
            .max(0.0);
        row(
            "缓冲",
            &if model.source_remote {
                format!("{:.1} s", lead_s.min(99.9))
            } else {
                "本地".to_string()
            },
            &mut y,
        );

        // Fixed-cell heat map: the whole video is one complete rectangle of
        // 144 equal slices, so a cell is always 1/144 of the duration. Colors:
        // played / loaded / current / not loaded; the legend below names them.
        let grid_top = y + 4.0 * s;
        let gap = 1.0 * s;
        let cell_w = ((content.width() - gap * (cols as f32 - 1.0)) / cols as f32).max(2.0);
        let cell_h = 9.0 * s;
        let grid_h = grid_rows as f32 * (cell_h + gap) - gap;
        let duration = model.duration_ms;
        let loaded_end = available_end_ms(model);
        let current = if duration > 0 {
            ((position as f64 / duration as f64) * cells as f64).floor() as usize
        } else {
            0
        }
        .min(cells - 1);
        for index in 0..cells {
            let column = index % cols;
            let line = index / cols;
            let left = content.left + column as f32 * (cell_w + gap);
            let top = grid_top + line as f32 * (cell_h + gap);
            let time = if duration > 0 {
                duration as f64 * (index as f64 + 0.5) / cells as f64
            } else {
                0.0
            };
            let brush = if duration == 0 {
                &self.chip
            } else if index == current {
                &self.heat_hot
            } else if time <= position as f64 {
                &self.heat
            } else if time <= loaded_end as f64 {
                &self.cache_fill
            } else {
                &self.chip
            };
            let cell = Rect {
                left,
                top,
                right: left + cell_w,
                bottom: top + cell_h,
            };
            let rounded = D2D1_ROUNDED_RECT {
                rect: to_d2d(cell),
                radiusX: 1.0 * s,
                radiusY: 1.0 * s,
            };
            self.d2d.FillRoundedRectangle(&rounded, brush);
        }
        // A hairline around the whole grid makes the 24×6 cells read as one
        // complete block instead of loose chips.
        self.stroke_rect(
            Rect {
                left: content.left,
                top: grid_top,
                right: content.left + cols as f32 * (cell_w + gap) - gap,
                bottom: grid_top + grid_h,
            },
            1.0 * s,
            &self.sidebar_panel_line,
        );
        y = grid_top + grid_h + 6.0 * s;

        // Legend: every state the heat map paints, with the same colors.
        if visible(y) {
            let mut x = content.left;
            let swatch = 7.0 * s;
            for (label, brush) in [
                ("已播放", &self.heat),
                ("已加载", &self.cache_fill),
                ("当前位置", &self.heat_hot),
                ("未加载", &self.chip),
            ] {
                let cell = Rect {
                    left: x,
                    top: y + 2.0 * s,
                    right: x + swatch,
                    bottom: y + 2.0 * s + swatch,
                };
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(cell),
                    radiusX: 1.5 * s,
                    radiusY: 1.5 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, brush);
                self.text(
                    label,
                    x + swatch + 4.0 * s,
                    y,
                    64.0 * s,
                    &self.small_format,
                    &self.sidebar_text_dim,
                );
                x += swatch + 4.0 * s + self.text_width(label, &self.small_format) + 8.0 * s;
            }
        }
        y += 18.0 * s;

        let total_bps = (model.video_bitrate + model.audio_bitrate) as f64;
        let bytes_per_sec = total_bps / 8.0;
        let total_bytes = if model.total_bytes > 0 {
            model.total_bytes
        } else {
            (bytes_per_sec * (model.duration_ms as f64 / 1000.0)).round() as u64
        };
        let video_host = if model.source_host.is_empty() {
            "直链".to_string()
        } else {
            ellipsize(&model.source_host, 24)
        };
        let audio_host = if model.audio_host.is_empty() {
            "无独立音轨".to_string()
        } else {
            ellipsize(&model.audio_host, 24)
        };
        row("总码率", &format_bitrate(total_bps as u64), &mut y);
        row("视频码率", &format_bitrate(model.video_bitrate), &mut y);
        row("音频码率", &format_bitrate(model.audio_bitrate), &mut y);
        row("总数据量", &format_bytes(total_bytes as f64), &mut y);
        row("下载线程", &model.download_streams.to_string(), &mut y);
        row(
            "下载速度",
            &format!("{}/s", format_bytes(model.net_bps.max(0.0))),
            &mut y,
        );
        row("视频源", &video_host, &mut y);
        row("音频源", &audio_host, &mut y);
        row(
            "模式",
            if model.source_remote {
                "远程"
            } else {
                "本地"
            },
            &mut y,
        );
    }

    /// One hollow 54×28 switch pill: only the border and the 开/关 ink change.
    unsafe fn sidebar_switch(&self, switch: Rect, on: bool, hot: bool, s: f32) {
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(switch),
            radiusX: 14.0 * s,
            radiusY: 14.0 * s,
        };
        self.d2d.DrawRoundedRectangle(
            &rounded,
            if on || hot {
                &self.accent_blue
            } else {
                &self.text_faint
            },
            1.5 * s,
            None,
        );
        self.text_centered(
            if on { "开" } else { "关" },
            switch,
            &self.comment_body_format,
            &self.text,
        );
    }

    unsafe fn draw_settings(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        let quality_value = if model.codec.is_empty() {
            model.quality_label.clone()
        } else {
            format!("{} · {}", model.quality_label, model.codec)
        };
        let rows: Vec<(&str, String, bool)> = if model.is_live {
            vec![
                ("直播画质", quality_value, false),
                (
                    "置顶",
                    if model.ontop { "开" } else { "关" }.to_string(),
                    true,
                ),
                (
                    "仅音频",
                    if model.audio_only { "开" } else { "关" }.to_string(),
                    true,
                ),
            ]
        } else {
            vec![
                ("当前画质", quality_value, true),
                (
                    "默认画质",
                    quality_height_label(model.player_quality_height).to_string(),
                    true,
                ),
                ("音轨", model.audio_label.clone(), true),
                ("速度", crate::speed::label(model.speed), true),
                ("插帧", interp_level_label(model), true),
                ("画质增强", model.enhance.label().to_string(), true),
                (
                    "置顶",
                    if model.ontop { "开" } else { "关" }.to_string(),
                    true,
                ),
                (
                    "仅音频",
                    if model.audio_only { "开" } else { "关" }.to_string(),
                    true,
                ),
            ]
        };
        let menu_state = if model.is_live {
            None
        } else {
            match model.settings_menu {
                Some(SettingsMenu::Quality) => Some((0usize, model.quality_rows.len())),
                Some(SettingsMenu::DefaultQuality) => {
                    Some((1usize, DEFAULT_QUALITY_HEIGHTS.len()))
                }
                Some(SettingsMenu::Audio) => Some((2usize, model.audio_rows.len())),
                Some(SettingsMenu::Interp) => Some((4usize, model.interp_rows.len())),
                Some(SettingsMenu::Enhance) => Some((5usize, model.enhance_rows.len())),
                None => None,
            }
        };
        let expanded_row = menu_state.map(|(row, _)| row);
        let expanded_h = menu_state
            .map(|(_, count)| settings_menu_height(s, count))
            .unwrap_or(0.0);
        let content_h = rows.len() as f32 * 48.0 * s + expanded_h;
        let visible_h = (content.height() - 8.0 * s).max(1.0);
        let max_scroll = (content_h - visible_h).max(0.0);
        let scroll = model.sidebar_scroll.clamp(0.0, max_scroll);
        for (index, (label, value, editable)) in rows.iter().enumerate() {
            let rect = layout.settings_row(index, scroll, expanded_row, expanded_h);
            if rect.bottom < content.top || rect.top > content.bottom - 8.0 * s {
                continue;
            }
            let editable = *editable;
            let open = expanded_row == Some(index);
            let hot = editable
                && model
                    .pointer
                    .map(|(x, y)| rect.contains(x, y))
                    .unwrap_or(false);
            if hot {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(rect),
                    radiusX: 10.0 * s,
                    radiusY: 10.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_hover);
            }
            self.text_box(
                label,
                Rect {
                    left: rect.left + 6.0 * s,
                    right: rect.left + 6.0 * s + rect.width() * 0.42,
                    ..rect
                },
                &self.comment_body_format,
                if open {
                    &self.sidebar_text
                } else {
                    &self.sidebar_text_dim
                },
                0,
            );
            self.text_box(
                value,
                Rect {
                    left: rect.right - 6.0 * s - rect.width() * 0.55,
                    right: rect.right - 6.0 * s,
                    ..rect
                },
                &self.comment_body_format,
                if editable {
                    &self.sidebar_text
                } else {
                    &self.sidebar_text_faint
                },
                2,
            );
        }
        if let Some((row, count)) = menu_state {
            self.draw_settings_menu_inline(model, layout, scroll, row, count);
        }
    }

    /// Inline 设置 menu: it occupies the list flow directly below its parent
    /// row, with no floating card, title, border or "Esc 关闭" line. Option rows
    /// use the same typography and hover treatment as the settings list.
    unsafe fn draw_settings_menu_inline(
        &self,
        model: &UiModel,
        layout: &Layout,
        scroll: f32,
        row: usize,
        count: usize,
    ) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        let menu = layout.settings_menu_rect(row, count, scroll);
        if menu.bottom < content.top || menu.top > content.bottom {
            return;
        }
        for index in 0..count {
            let rect = layout.settings_menu_option(row, count, scroll, index);
            if rect.bottom < content.top || rect.top > content.bottom - 4.0 * s {
                continue;
            }
            let (label, detail, current, locked) = match model.settings_menu {
                Some(SettingsMenu::DefaultQuality) => {
                    let Some(height) = DEFAULT_QUALITY_HEIGHTS.get(index) else {
                        continue;
                    };
                    (
                        quality_height_label(*height),
                        if *height == 0 { "始终最高" } else { "上限" },
                        model.player_quality_height == *height,
                        false,
                    )
                }
                Some(SettingsMenu::Quality) => {
                    let Some(item) = model.quality_rows.get(index) else {
                        continue;
                    };
                    (
                        item.label.as_str(),
                        item.detail.as_str(),
                        item.current,
                        item.locked,
                    )
                }
                Some(SettingsMenu::Audio) => {
                    let Some(item) = model.audio_rows.get(index) else {
                        continue;
                    };
                    (
                        item.label.as_str(),
                        item.detail.as_str(),
                        item.current,
                        false,
                    )
                }
                Some(SettingsMenu::Interp) => {
                    let Some(item) = model.interp_rows.get(index) else {
                        continue;
                    };
                    (
                        item.label.as_str(),
                        item.detail.as_str(),
                        item.current,
                        false,
                    )
                }
                Some(SettingsMenu::Enhance) => {
                    let Some(item) = model.enhance_rows.get(index) else {
                        continue;
                    };
                    (
                        item.label.as_str(),
                        item.detail.as_str(),
                        item.current,
                        false,
                    )
                }
                None => return,
            };
            let hot = model
                .pointer
                .map(|(x, y)| rect.contains(x, y))
                .unwrap_or(false);
            if hot && !locked {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(rect),
                    radiusX: 8.0 * s,
                    radiusY: 8.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_hover);
            }
            if current {
                self.d2d.FillEllipse(
                    &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                        point: Vector2::new(rect.left + 10.0 * s, (rect.top + rect.bottom) * 0.5),
                        radiusX: 2.5 * s,
                        radiusY: 2.5 * s,
                    },
                    &self.accent,
                );
            }
            self.text_box(
                label,
                Rect {
                    left: rect.left + 18.0 * s,
                    right: rect.left + 18.0 * s + rect.width() * 0.64,
                    ..rect
                },
                &self.comment_body_format,
                if locked {
                    &self.sidebar_text_faint
                } else if current {
                    &self.accent
                } else {
                    &self.sidebar_text
                },
                0,
            );
            self.text_box(
                detail,
                Rect {
                    left: rect.right - 6.0 * s - rect.width() * 0.40,
                    right: rect.right - 6.0 * s,
                    ..rect
                },
                &self.comment_body_format,
                if locked {
                    &self.sidebar_text_faint
                } else {
                    &self.sidebar_text_dim
                },
                2,
            );
        }
    }

    unsafe fn precision_chip(&self, rect: Rect, label: &str, active: bool, s: f32) {
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 8.0 * s,
            radiusY: 8.0 * s,
        };
        self.d2d.FillRoundedRectangle(
            &rounded,
            if active {
                &self.accent_blue
            } else {
                &self.chip
            },
        );
        self.d2d
            .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
        self.text_centered(
            label,
            rect,
            &self.comment_body_format,
            if active { &self.accent_fg } else { &self.text },
        );
    }

    unsafe fn precision_clear_chip(&self, rect: Rect, label: &str, s: f32) {
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 8.0 * s,
            radiusY: 8.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.danger_fill);
        self.d2d
            .DrawRoundedRectangle(&rounded, &self.danger_line, 1.0 * s, None);
        self.text_centered(label, rect, &self.comment_name_format, &self.danger_soft);
    }

    /// One paper card that morphs between the toolbar and a bottom panel
    /// (precision seek or playback speed). The toolbar content fades out over
    /// the first part of the morph, the panel content fades in over the
    /// second, and the panel rows are clipped by the growing card so they are
    /// revealed from the bottom.
    unsafe fn draw_bottom_panel(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let mix = model.precision_mix.clamp(0.0, 1.0);
        let ease = mix * mix * (3.0 - 2.0 * mix);
        let target = if model.panel == 2 {
            layout.speed_panel
        } else {
            layout.precision
        };
        let card = lerp_rect(layout.panel, target, ease);
        if model.panel == 2 {
            self.draw_home_panel_plain(card, 12.0 * s, card, model.light_sidebar);
        } else {
            self.draw_home_panel(card, 12.0 * s, card, model.light_sidebar);
        }

        let toolbar_opacity = (1.0 - ease / 0.42).clamp(0.0, 1.0);
        if model.chrome_shown && toolbar_opacity > 0.01 {
            if toolbar_opacity >= 0.995 {
                self.draw_toolbar_contents(model, layout);
            } else {
                let layer = self.d2d.CreateLayer(None).ok();
                if let Some(layer) = &layer {
                    let params = windows::Win32::Graphics::Direct2D::D2D1_LAYER_PARAMETERS1 {
                        contentBounds: to_d2d(layout.stage),
                        geometricMask: ManuallyDrop::new(None),
                        maskAntialiasMode:
                            windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                        maskTransform: Matrix3x2::identity(),
                        opacity: toolbar_opacity,
                        opacityBrush: ManuallyDrop::new(None),
                        layerOptions: windows::Win32::Graphics::Direct2D::D2D1_LAYER_OPTIONS1_NONE,
                    };
                    self.d2d.PushLayer(&params, layer);
                }
                self.draw_toolbar_contents(model, layout);
                if layer.is_some() {
                    self.d2d.PopLayer();
                }
            }
        }

        let precision_opacity = ((ease - 0.18) / 0.82).clamp(0.0, 1.0);
        if precision_opacity > 0.01 {
            // The precision panel is wider than the pill: clip its full-width
            // layout to the growing card so the extra columns unfold smoothly
            // instead of painting outside the paper while it morphs.
            let clip = to_d2d(card);
            self.d2d
                .PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            if precision_opacity >= 0.995 {
                self.draw_panel_content(model, layout);
            } else {
                let layer = self.d2d.CreateLayer(None).ok();
                if let Some(layer) = &layer {
                    let params = windows::Win32::Graphics::Direct2D::D2D1_LAYER_PARAMETERS1 {
                        contentBounds: to_d2d(card),
                        geometricMask: ManuallyDrop::new(None),
                        maskAntialiasMode:
                            windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                        maskTransform: Matrix3x2::identity(),
                        opacity: precision_opacity,
                        opacityBrush: ManuallyDrop::new(None),
                        layerOptions: windows::Win32::Graphics::Direct2D::D2D1_LAYER_OPTIONS1_NONE,
                    };
                    self.d2d.PushLayer(&params, layer);
                }
                self.draw_panel_content(model, layout);
                if layer.is_some() {
                    self.d2d.PopLayer();
                }
            }
            self.d2d.PopAxisAlignedClip();
        }
    }

    unsafe fn draw_panel_content(&mut self, model: &UiModel, layout: &Layout) {
        if model.panel == 2 {
            self.draw_speed(model, layout);
        } else {
            self.draw_precision(model, layout);
        }
    }

    /// 36px coral play/pause cell shared by the toolbar and the precision
    /// panel, so both controls are pixel-identical.
    unsafe fn draw_play_button(&self, rect: Rect, playing: bool, s: f32) {
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 10.0 * s,
            radiusY: 10.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.accent);
        let center = Vector2::new(
            (rect.left + rect.right) * 0.5,
            (rect.top + rect.bottom) * 0.5,
        );
        if playing {
            for sign in [-1.0f32, 1.0] {
                self.fill(
                    Rect {
                        left: center.X + sign * 4.0 * s - 1.2 * s,
                        top: center.Y - 6.0 * s,
                        right: center.X + sign * 4.0 * s + 1.2 * s,
                        bottom: center.Y + 6.0 * s,
                    },
                    &self.accent_fg,
                );
            }
        } else {
            let scale = 10.0 * s;
            let transform = Matrix3x2 {
                M11: scale,
                M12: 0.0,
                M21: 0.0,
                M22: scale,
                M31: center.X - 4.0 * s,
                M32: center.Y - 6.0 * s,
            };
            self.render_target.SetTransform(&transform);
            if let Ok(geometry) = self.play_geometry.cast::<ID2D1Geometry>() {
                if let Ok(brush) = self.accent_fg.cast::<ID2D1Brush>() {
                    let _ = self
                        .render_target
                        .FillGeometry(&geometry, &brush, None::<&ID2D1Brush>);
                }
            }
            self.render_target.SetTransform(&Matrix3x2::identity());
        }
    }

    /// Playback speed: one flat picker wheel filling the toolbar. The wheel
    /// owns the drag and wheel input; there are no presets or extra readout.
    unsafe fn draw_speed(&mut self, model: &UiModel, layout: &Layout) {
        let speed = crate::speed::clamp(model.speed_preview);
        self.draw_speed_wheel(layout, speed, model.speed_base, model.tuner_offset);
    }

    /// A flat horizontal picker wheel modelled on the precision frame ruler:
    /// every speed stop owns one tick, the reel travels under a fixed centre
    /// band, and a tall red aim line marks the stop under the middle. The
    /// selected value is read out above the ruler, so the red line never
    /// crosses a label.
    unsafe fn draw_speed_wheel(&self, layout: &Layout, speed: f32, base: f32, offset: f32) {
        let s = layout.scale;
        let card = layout.speed_wheel;
        let centre = (card.left + card.right) * 0.5;
        let gap = crate::speed::WHEEL_GAP * s;
        let base_index = crate::speed::index_of(base) as f32;

        // Fixed readout above the ruler. The ruler itself only carries the
        // scale, exactly like the frame wheel in the precision panel.
        self.text_centered(
            &crate::speed::label(speed),
            Rect {
                left: centre - 60.0 * s,
                top: card.top + 3.0 * s,
                right: centre + 60.0 * s,
                bottom: card.top + 27.0 * s,
            },
            &self.speed_value_format,
            &self.accent_blue,
        );

        let strip = Rect {
            left: card.left + 12.0 * s,
            top: card.top + 28.0 * s,
            right: card.right - 12.0 * s,
            bottom: card.bottom - 8.0 * s,
        };
        let strip_round = D2D1_ROUNDED_RECT {
            rect: to_d2d(strip),
            radiusX: 10.0 * s,
            radiusY: 10.0 * s,
        };
        self.d2d.FillRoundedRectangle(&strip_round, &self.chip);
        self.d2d
            .DrawRoundedRectangle(&strip_round, &self.panel_border, 1.0 * s, None);

        let label_top = strip.top + 1.0 * s;
        let label_bottom = strip.top + 15.0 * s;
        let tick_top = strip.top + 18.0 * s;
        let tick_bottom = strip.bottom - 6.0 * s;

        let clip = windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
            left: strip.left + 1.0 * s,
            top: strip.top + 1.0 * s,
            right: strip.right - 1.0 * s,
            bottom: strip.bottom - 1.0 * s,
        };
        self.d2d
            .PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

        for (index, value) in crate::speed::TICKS.iter().enumerate() {
            let x = centre + (index as f32 - base_index) * gap + offset;
            if x < strip.left - 30.0 * s || x > strip.right + 30.0 * s {
                continue;
            }
            let at_aim = (x - centre).abs() < 1.0 * s;
            if !at_aim {
                let major = value.fract().abs() < 0.001;
                let distance = (x - centre).abs() / gap;
                let (top, width, brush): (f32, f32, &ID2D1SolidColorBrush) = if major {
                    (tick_top, 1.6 * s, &self.accent_blue)
                } else if distance < 1.6 {
                    (tick_top + 5.0 * s, 1.0 * s, &self.text_dim)
                } else if distance < 3.6 {
                    (tick_top + 5.0 * s, 1.0 * s, &self.text_faint)
                } else {
                    (tick_top + 5.0 * s, 1.0 * s, &self.sidebar_text_faint)
                };
                self.line(x, top, x, tick_bottom, width, brush);
                if major {
                    self.line(
                        x - 3.5 * s,
                        tick_top,
                        x + 3.5 * s,
                        tick_top,
                        1.0 * s,
                        &self.accent_blue,
                    );
                }
            }
            if !at_aim {
                let distance = (x - centre).abs() / gap;
                let brush = if distance < 1.6 {
                    &self.text_dim
                } else if distance < 3.6 {
                    &self.text_faint
                } else {
                    &self.sidebar_text_faint
                };
                self.text_centered(
                    &crate::speed::label(*value),
                    Rect {
                        left: x - 26.0 * s,
                        top: label_top,
                        right: x + 26.0 * s,
                        bottom: label_bottom,
                    },
                    &self.speed_scale_format,
                    brush,
                );
            }
        }
        self.d2d.PopAxisAlignedClip();

        // Full-height centred aim line: the tick under it is the stop that
        // will be committed. The centre label is omitted, so the line never
        // cuts through the selected value at rest.
        self.line(
            centre,
            strip.top + 2.0 * s,
            centre,
            strip.bottom - 2.0 * s,
            2.0 * s,
            &self.aim_line,
        );
    }

    /// Precision seek: a two-sided timecode / frame readout, a full-width
    /// heatmap, A/B/clear, and a gliding frame wheel. This is the content of
    /// the card that the toolbar morphs into; it deliberately has no title,
    /// no close button and no transport (spacebar and the picture own
    /// play/pause).
    unsafe fn draw_precision(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let panel = layout.precision;

        let position = model.drag_preview.unwrap_or(model.position_ms);
        let duration = model.duration_ms;
        let fps = if model.source_fps > 1.0 {
            model.source_fps
        } else {
            30.0
        };
        let frame_ms = (1000.0 / fps).max(0.01);
        let total_frames = if duration > 0 {
            (duration as f64 / frame_ms as f64).round() as u64
        } else {
            0
        };
        let current_frame = if total_frames > 0 {
            ((position as f64 / frame_ms as f64).round() as u64).min(total_frames)
        } else {
            0
        };

        // One typographic header with matching labels on both sides:
        // 时间 03:21.461 / 14:16           帧 6044 / 25680
        // The clock value is the large face, its millisecond tail and the two
        // totals are secondary, and the labels / slashes are the quietest.
        // Every piece shares one baseline; there is no pill or transport.
        let content_left = panel.left + 20.0 * s;
        let content_right = panel.right - 20.0 * s;
        let baseline = panel.top + 29.0 * s;

        let time_label = "时间";
        let time_label_w = self.text_width(time_label, &self.small_format);
        self.text_on_baseline(
            time_label,
            content_left,
            baseline,
            time_label_w + 2.0 * s,
            &self.small_format,
            &self.text_faint,
        );

        let time_left = content_left + time_label_w + 9.0 * s;
        let clock_base = crate::format::format_time(position);
        let clock_ms = format!(".{:03}", position % 1000);
        let clock_base_w = self.text_width(&clock_base, &self.precision_clock_format);
        self.text_on_baseline(
            &clock_base,
            time_left,
            baseline,
            clock_base_w + 2.0 * s,
            &self.precision_clock_format,
            &self.text,
        );
        let clock_ms_left = time_left + clock_base_w + 1.0 * s;
        let clock_ms_w = self.text_width(&clock_ms, &self.precision_frame_format);
        self.text_on_baseline(
            &clock_ms,
            clock_ms_left,
            baseline,
            clock_ms_w + 2.0 * s,
            &self.precision_frame_format,
            &self.text_dim,
        );

        let slash_left = clock_ms_left + clock_ms_w + 10.0 * s;
        let slash_w = self.text_width("/", &self.small_format);
        self.text_on_baseline(
            "/",
            slash_left,
            baseline,
            slash_w + 2.0 * s,
            &self.small_format,
            &self.text_faint,
        );
        let total = crate::format::format_time(duration);
        let total_left = slash_left + slash_w + 9.0 * s;
        let total_w = self.text_width(&total, &self.small_format);
        self.text_on_baseline(
            &total,
            total_left,
            baseline,
            total_w + 2.0 * s,
            &self.small_format,
            &self.text_dim,
        );

        let frames_total = total_frames.to_string();
        let frames_total_w = self.text_width(&frames_total, &self.small_format);
        let frames_total_left = content_right - frames_total_w;
        self.text_on_baseline(
            &frames_total,
            frames_total_left,
            baseline,
            frames_total_w + 2.0 * s,
            &self.small_format,
            &self.text_dim,
        );
        let frames_slash_right = frames_total_left - 6.0 * s;
        let frames_slash_w = self.text_width("/", &self.small_format);
        self.text_on_baseline(
            "/",
            frames_slash_right - frames_slash_w,
            baseline,
            frames_slash_w + 2.0 * s,
            &self.small_format,
            &self.text_faint,
        );
        let frame_value = current_frame.to_string();
        let frame_value_w = self.text_width(&frame_value, &self.precision_frame_format);
        let frame_value_left = frames_slash_right - frames_slash_w - 8.0 * s - frame_value_w;
        self.text_on_baseline(
            &frame_value,
            frame_value_left,
            baseline,
            frame_value_w + 2.0 * s,
            &self.precision_frame_format,
            &self.text,
        );
        let frame_label = "帧";
        let frame_label_w = self.text_width(frame_label, &self.small_format);
        let frame_label_left = frame_value_left - 8.0 * s - frame_label_w;
        self.text_on_baseline(
            frame_label,
            frame_label_left,
            baseline,
            frame_label_w + 2.0 * s,
            &self.small_format,
            &self.text_faint,
        );


        // Full-width heatmap track.
        let track = layout.precision_track;
        let track_round = D2D1_ROUNDED_RECT {
            rect: to_d2d(track),
            radiusX: 9.0 * s,
            radiusY: 9.0 * s,
        };
        self.d2d.FillRoundedRectangle(&track_round, &self.chip);
        self.d2d
            .DrawRoundedRectangle(&track_round, &self.panel_border, 1.0 * s, None);
        let fraction = if duration > 0 {
            (position as f32 / duration as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let fraction_at = |time: u64| {
            if duration > 0 {
                (time as f32 / duration as f32).clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        let range = match (model.ab_a, model.ab_b) {
            (Some(a), Some(b)) if b > a => Some((a, b)),
            _ => None,
        };
        if let Some((a, b)) = range {
            let xa = track.left + fraction_at(a) * track.width();
            let xb = track.left + fraction_at(b) * track.width();
            self.fill(
                Rect {
                    left: xa,
                    top: track.top + 2.0 * s,
                    right: xb,
                    bottom: track.bottom - 2.0 * s,
                },
                &self.loop_fill,
            );
        }
        let count = model.heatmap.len().max(1);
        let gap = 1.0 * s;
        let bar_w = ((track.width() - gap * (count - 1) as f32) / count as f32).max(1.0);
        for (index, value) in model.heatmap.iter().enumerate() {
            if *value == 0 {
                continue;
            }
            let bar_h = (2.0 * s + (*value as f32 / 255.0) * (track.height() - 7.0 * s))
                .min(track.height() - 4.0 * s);
            let left = track.left + index as f32 * (bar_w + gap);
            self.fill(
                Rect {
                    left,
                    top: track.bottom - 2.0 * s - bar_h,
                    right: left + bar_w,
                    bottom: track.bottom - 2.0 * s,
                },
                if *value > 170 {
                    &self.heat_hot
                } else {
                    &self.heat
                },
            );
        }
        let play_x = track.left + fraction * track.width();
        self.line(
            track.left,
            track.bottom - 2.0 * s,
            play_x,
            track.bottom - 2.0 * s,
            2.0 * s,
            &self.accent_blue,
        );
        self.line(
            play_x,
            track.top + 3.0 * s,
            play_x,
            track.bottom - 3.0 * s,
            1.5 * s,
            &self.accent_blue,
        );
        for time in [model.ab_a, model.ab_b].into_iter().flatten() {
            let x = (track.left + fraction_at(time) * track.width())
                .clamp(track.left + 6.0 * s, track.right - 6.0 * s);
            self.line(
                x,
                track.top + 3.0 * s,
                x,
                track.bottom - 3.0 * s,
                2.0 * s,
                &self.accent_blue,
            );
        }

        // A / B / 清空: A and B share the row; 清空 is the visible tail action.
        let a_label = model
            .ab_a
            .map(|time| format!("A {}", crate::format::format_time_ms(time)))
            .unwrap_or_else(|| "A 起点".to_string());
        let b_label = model
            .ab_b
            .map(|time| format!("B {}", crate::format::format_time_ms(time)))
            .unwrap_or_else(|| "B 终点".to_string());
        self.precision_chip(layout.precision_a, &a_label, model.ab_a.is_some(), s);
        self.precision_chip(layout.precision_b, &b_label, model.ab_b.is_some(), s);
        self.precision_clear_chip(layout.precision_clear, "清空", s);

        // Full-width,endless frame ruler. The strip travels with the pointer
        // for the whole drag; tick indices are derived from the unbounded
        // offset so the scale never runs out of marks. Every fourth blue tick
        // carries a top graduation cap and its frame offset.
        let wheel = layout.precision_wheel;
        let wheel_round = D2D1_ROUNDED_RECT {
            rect: to_d2d(wheel),
            radiusX: 10.0 * s,
            radiusY: 10.0 * s,
        };
        self.d2d.FillRoundedRectangle(&wheel_round, &self.chip);
        self.d2d
            .DrawRoundedRectangle(&wheel_round, &self.panel_border, 1.0 * s, None);
        let centre = (wheel.left + wheel.right) * 0.5;
        let label_top = wheel.top + 2.0 * s;
        let label_bottom = wheel.top + 15.0 * s;
        let tick_top = wheel.top + 18.0 * s;
        let tick_bottom = wheel.bottom - 7.0 * s;
        let band = Rect {
            left: centre - 24.0 * s,
            top: wheel.top + 2.0 * s,
            right: centre + 24.0 * s,
            bottom: wheel.bottom - 2.0 * s,
        };
        let band_round = D2D1_ROUNDED_RECT {
            rect: to_d2d(band),
            radiusX: 7.0 * s,
            radiusY: 7.0 * s,
        };
        self.d2d.FillRoundedRectangle(&band_round, &self.loop_fill);

        let clip = windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
            left: wheel.left + 1.0 * s,
            top: wheel.top + 1.0 * s,
            right: wheel.right - 1.0 * s,
            bottom: wheel.bottom - 1.0 * s,
        };
        self.d2d
            .PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        let ridge_gap = 12.0 * s;
        let offset = model.tuner_offset;
        let (first, last) = tuner_tick_range(wheel.width() * 0.5, offset, ridge_gap);
        for index in first..=last {
            let x = centre + index as f32 * ridge_gap + offset;
            if x < wheel.left - 24.0 * s || x > wheel.right + 24.0 * s {
                continue;
            }
            let major = index.rem_euclid(4) == 0;
            let medium = index.rem_euclid(2) == 0;
            // The red aim needle owns the exact centre. Leave the scale's own
            // graduation out there, otherwise its blue cap peeks out from
            // behind the needle as a second, slightly offset "T".
            let at_aim = (x - centre).abs() < 1.0 * s;
            if !at_aim {
                let (top, width, brush): (f32, f32, &ID2D1SolidColorBrush) = if major {
                    (tick_top, 1.6 * s, &self.accent_blue)
                } else if medium {
                    (tick_top + 5.0 * s, 1.0 * s, &self.text_dim)
                } else {
                    (tick_top + 9.0 * s, 1.0 * s, &self.text_faint)
                };
                self.line(x, top, x, tick_bottom, width, brush);
                if major {
                    // A small horizontal graduation above the blue tick.
                    self.line(
                        x - 3.5 * s,
                        tick_top,
                        x + 3.5 * s,
                        tick_top,
                        1.0 * s,
                        &self.accent_blue,
                    );
                }
            }
            if major && x >= wheel.left + 26.0 * s && x <= wheel.right - 26.0 * s {
                let frames = index as i64 * model.frame_step as i64;
                let label = if frames > 0 {
                    format!("+{frames}")
                } else {
                    format!("{frames}")
                };
                self.text_centered(
                    &label,
                    Rect {
                        left: x - 24.0 * s,
                        top: label_top,
                        right: x + 24.0 * s,
                        bottom: label_bottom,
                    },
                    &self.hud_format,
                    &self.text_dim,
                );
            }
        }
        self.d2d.PopAxisAlignedClip();

        // One long red needle marks the exact seek point. It runs past the
        // scale's ticks at both ends and is the only mark at the centre; the
        // scrolling strip always returns here.
        self.line(
            centre,
            label_bottom,
            centre,
            wheel.bottom - 2.0 * s,
            2.0 * s,
            &self.aim_line,
        );

        // Frame-step dropdown.
        let field = layout.precision_step;
        let open = model.precision_step_open;
        let field_round = D2D1_ROUNDED_RECT {
            rect: to_d2d(field),
            radiusX: 10.0 * s,
            radiusY: 10.0 * s,
        };
        self.d2d.FillRoundedRectangle(
            &field_round,
            if open { &self.sidebar_chip } else { &self.chip },
        );
        self.d2d.DrawRoundedRectangle(
            &field_round,
            if open {
                &self.accent_blue
            } else {
                &self.panel_border
            },
            if open { 1.2 * s } else { 1.0 * s },
            None,
        );
        self.text(
            &format!("{} 帧", model.frame_step),
            field.left + 12.0 * s,
            field.top + (field.height() - 17.0 * s) * 0.5,
            field.width() - 34.0 * s,
            &self.comment_body_format,
            if open { &self.accent_blue } else { &self.text },
        );
        let chevron_cx = field.right - 16.0 * s;
        let chevron_cy = field.top + field.height() * 0.5;
        let chevron = if open {
            [(-3.0, 2.0), (0.0, -1.0), (3.0, 2.0)]
        } else {
            [(-3.0, -1.0), (0.0, 2.0), (3.0, -1.0)]
        };
        let chevron_brush = if open {
            &self.accent_blue
        } else {
            &self.text_dim
        };
        self.line(
            chevron_cx + chevron[0].0 * s,
            chevron_cy + chevron[0].1 * s,
            chevron_cx + chevron[1].0 * s,
            chevron_cy + chevron[1].1 * s,
            1.4 * s,
            chevron_brush,
        );
        self.line(
            chevron_cx + chevron[1].0 * s,
            chevron_cy + chevron[1].1 * s,
            chevron_cx + chevron[2].0 * s,
            chevron_cy + chevron[2].1 * s,
            1.4 * s,
            chevron_brush,
        );
        if open {
            let menu = layout.precision_step_menu;
            self.draw_home_panel(menu, 10.0 * s, menu, model.light_sidebar);
            for (index, rect) in layout.precision_step_rows.iter().enumerate() {
                let step = [1u64, 2, 5, 10][index];
                let current = model.frame_step == step;
                let hot = model
                    .pointer
                    .map(|(x, y)| rect.contains(x, y))
                    .unwrap_or(false);
                if hot && !current {
                    let rounded = D2D1_ROUNDED_RECT {
                        rect: to_d2d(*rect),
                        radiusX: 7.0 * s,
                        radiusY: 7.0 * s,
                    };
                    self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_hover);
                }
                if current {
                    self.d2d.FillEllipse(
                        &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                            point: Vector2::new(
                                rect.left + 10.0 * s,
                                (rect.top + rect.bottom) * 0.5,
                            ),
                            radiusX: 2.5 * s,
                            radiusY: 2.5 * s,
                        },
                        &self.accent_blue,
                    );
                }
                self.text(
                    &format!("{} 帧", step),
                    rect.left + 18.0 * s,
                    rect.top + (rect.height() - 17.0 * s) * 0.5,
                    rect.width() - 24.0 * s,
                    &self.comment_body_format,
                    if current {
                        &self.accent_blue
                    } else {
                        &self.text
                    },
                );
            }
        }
    }

    /// The toolbar is the same home-page paper as the sidebar lane: an opaque
    /// warm-paper pill with the three wallpaper glows and the 3px dot texture,
    /// clipped by the pill's rounded corners. No video bleed, no glass blur.
    /// Rounded home-page surface used by the toolbar and the loading/status
    /// cards. Base paper, the three wallpaper glows, the dot grid and a 1px
    /// hairline are clipped by the rounded rect, so the video never bleeds
    /// through and every floating panel shares one material.
    unsafe fn draw_home_panel(&self, rect: Rect, radius: f32, origin: Rect, light: bool) {
        self.draw_home_panel_stroke(rect, radius, origin, light, true);
    }

    /// The speed card is the whole toolbar: same paper, no rim.
    unsafe fn draw_home_panel_plain(&self, rect: Rect, radius: f32, origin: Rect, light: bool) {
        self.draw_home_panel_stroke(rect, radius, origin, light, false);
    }

    unsafe fn draw_home_panel_stroke(
        &self,
        rect: Rect,
        radius: f32,
        origin: Rect,
        light: bool,
        stroke: bool,
    ) {
        let s = self.layout.scale;
        let panel = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: radius,
            radiusY: radius,
        };
        self.d2d
            .FillRoundedRectangle(&panel, &self.sidebar_wallpaper);
        let glows = if light {
            &self.sidebar_glow_light
        } else {
            &self.sidebar_glow_dark
        };
        let width = origin.width().max(1.0);
        let height = origin.height().max(1.0);
        let geometry = [
            (0.06, -0.12, 0.90, 0.95),
            (1.04, 0.04, 0.80, 0.70),
            (0.64, 1.08, 0.70, 0.80),
        ];
        for (brush, (cx, cy, rx, ry)) in glows.iter().zip(geometry) {
            brush.SetCenter(Vector2::new(
                origin.left + width * cx,
                origin.top + height * cy,
            ));
            brush.SetRadiusX(width * rx);
            brush.SetRadiusY(height * ry);
            let base: &ID2D1Brush = brush;
            self.d2d.FillRoundedRectangle(&panel, base);
        }
        let dots: &ID2D1Brush = if light {
            &self.sidebar_dots_light
        } else {
            &self.sidebar_dots_dark
        };
        self.d2d.FillRoundedRectangle(&panel, dots);
        if stroke {
            self.d2d
                .DrawRoundedRectangle(&panel, &self.sidebar_panel_line, 1.0 * s, None);
        }
    }

    /// Accent arc on a faint track. The arc rotates once the caller advances
    /// `phase`; it is used by the centered loading prompt.
    unsafe fn draw_loading_ring(&self, center: Vector2, radius: f32, thickness: f32, phase: f32) {
        let track = windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
            point: center,
            radiusX: radius,
            radiusY: radius,
        };
        self.d2d.DrawEllipse(
            &track,
            &self.sidebar_panel_line,
            thickness,
            Some(&self.icon_stroke),
        );
        let start = phase - std::f32::consts::FRAC_PI_2;
        let sweep = 4.4;
        let end = start + sweep;
        let point = |angle: f32| {
            Vector2::new(
                center.X + angle.cos() * radius,
                center.Y + angle.sin() * radius,
            )
        };
        let Ok(geometry) = self._d2d_factory.CreatePathGeometry() else {
            return;
        };
        let Ok(sink) = geometry.Open() else {
            return;
        };
        sink.BeginFigure(point(start), D2D1_FIGURE_BEGIN_FILLED);
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: point(end),
            size: D2D_SIZE_F {
                width: radius,
                height: radius,
            },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_LARGE,
        });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        let _ = sink.Close();
        let Ok(geometry) = geometry.cast::<ID2D1Geometry>() else {
            return;
        };
        let Ok(brush) = self.accent.cast::<ID2D1Brush>() else {
            return;
        };
        self.render_target
            .DrawGeometry(&geometry, &brush, thickness, Some(&self.icon_stroke));
    }

    /// Large centered prompt while the picture is loading or buffering.
    unsafe fn draw_loading_prompt(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let stage = layout.stage;
        let card_w = (300.0 * s).min((stage.width() - 40.0 * s).max(220.0 * s));
        let card_h = 148.0 * s;
        let cx = (stage.left + stage.right) * 0.5;
        let cy = (stage.top + stage.bottom) * 0.5;
        let card = Rect {
            left: cx - card_w * 0.5,
            top: cy - card_h * 0.5,
            right: cx + card_w * 0.5,
            bottom: cy + card_h * 0.5,
        };
        self.draw_home_panel(card, 16.0 * s, card, model.light_sidebar);
        let phase = (self.frames as f32 * 0.085) % std::f32::consts::TAU;
        // 正在缓冲 keeps only the ring and the title, centered as one group in
        // the card. Loading / seeking still explain themselves underneath.
        if model.buffering {
            self.draw_loading_ring(
                Vector2::new(cx, card.top + 62.0 * s),
                16.0 * s,
                2.4 * s,
                phase,
            );
            self.text_centered(
                "正在缓冲",
                Rect {
                    left: card.left + 18.0 * s,
                    top: card.top + 82.0 * s,
                    right: card.right - 18.0 * s,
                    bottom: card.top + 106.0 * s,
                },
                &self.tab_active_format,
                &self.sidebar_text,
            );
            return;
        }
        self.draw_loading_ring(
            Vector2::new(cx, card.top + 40.0 * s),
            16.0 * s,
            2.4 * s,
            phase,
        );
        let (title, subtitle) = if model.status.contains("定位") {
            ("正在定位", "正在跳转到目标时间…")
        } else if model.status.is_empty() {
            ("正在加载视频", "正在准备播放…")
        } else {
            ("正在加载视频", model.status.as_str())
        };
        self.text_centered(
            title,
            Rect {
                left: card.left + 18.0 * s,
                top: card.top + 66.0 * s,
                right: card.right - 18.0 * s,
                bottom: card.top + 90.0 * s,
            },
            &self.tab_active_format,
            &self.sidebar_text,
        );
        self.text_centered(
            subtitle,
            Rect {
                left: card.left + 18.0 * s,
                top: card.top + 92.0 * s,
                right: card.right - 18.0 * s,
                bottom: card.top + 116.0 * s,
            },
            &self.hint_format,
            &self.sidebar_text_dim,
        );
    }

    /// Accent frame around the entire client while paused: a crisp 4px line
    /// plus a 1px inner hairline. There is no inner halo band, so the line
    /// sits directly against the picture; the OS window shadow is the only
    /// soft edge. The radius follows the window corners in windowed mode and
    /// goes square in fullscreen / maximize.
    ///
    /// The player window is clipped by a GDI region with a 16 logical-pixel
    /// corner (`window::apply_player_region`). Every arc here is concentric
    /// with that region: a rounded rect inset by `d` gets radius `16*s - d`,
    /// so the blue stroke's outer edge lands on the window edge instead of
    /// leaving a widening wedge at each corner.
    unsafe fn draw_pause_frame(&self, layout: &Layout, model: &UiModel) {
        let s = layout.scale;
        let width = layout.width;
        let height = layout.height;
        let stroke = 4.0 * s;
        let inset = stroke * 0.5 + 0.1 * s;
        if width <= inset * 2.0 || height <= inset * 2.0 {
            return;
        }
        let window_radius = if model.fullscreen || model.maximized {
            0.0
        } else {
            16.0 * s
        };
        let radius = (window_radius - inset).max(0.0);
        let frame = D2D1_ROUNDED_RECT {
            rect: to_d2d(Rect {
                left: inset,
                top: inset,
                right: width - inset,
                bottom: height - inset,
            }),
            radiusX: radius,
            radiusY: radius,
        };
        // No inner halo band: the old 7px glow showed as a pale strip between
        // the frame and the picture, which read as a corner gap. The window
        // shadow supplies the soft outer edge instead.
        self.d2d
            .DrawRoundedRectangle(&frame, &self.accent_blue, stroke, None);
        let hair_inset = inset + 4.5 * s;
        let hair_radius = (window_radius - hair_inset).max(0.0);
        let inner = D2D1_ROUNDED_RECT {
            rect: to_d2d(Rect {
                left: hair_inset,
                top: hair_inset,
                right: width - hair_inset,
                bottom: height - hair_inset,
            }),
            radiusX: hair_radius,
            radiusY: hair_radius,
        };
        self.d2d
            .DrawRoundedRectangle(&inner, &self.panel_border, 1.0 * s, None);
    }

    unsafe fn draw_toolbar_contents(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;

        // Seek band: 4px track, accent fill, 8px knob.
        let line_top = layout.track.top + (layout.track.height() - 4.0 * s) * 0.5;
        let track = Rect {
            left: layout.track.left,
            top: line_top,
            right: layout.track.right,
            bottom: line_top + 4.0 * s,
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(track),
            radiusX: 2.0 * s,
            radiusY: 2.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.track);
        let fraction = if model.duration_ms > 0 {
            (model.drag_preview.unwrap_or(model.position_ms) as f32 / model.duration_ms as f32)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let filled_right = layout.track.left + layout.track.width() * fraction;
        // Cached/buffered span: the real cache progress (or the decoded ring
        // when a direct URL has no disk cache) drawn under the played fill, so
        // the toolbar shows how much is already available ahead of the knob.
        // Local files are complete, and the disk cache reports its whole
        // downloaded span instead of only the reader's look-ahead.
        if model.duration_ms > 0 {
            let buffered_end = available_end_ms(model);
            let buffered_fraction =
                (buffered_end as f32 / model.duration_ms as f32).clamp(fraction, 1.0);
            if buffered_fraction > fraction + 0.0005 {
                let buffered = Rect {
                    left: layout.track.left,
                    top: track.top,
                    right: layout.track.left + layout.track.width() * buffered_fraction,
                    bottom: track.bottom,
                };
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(buffered),
                    radiusX: 2.0 * s,
                    radiusY: 2.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.cache_fill);
            }
        }
        let filled = Rect {
            left: track.left,
            top: track.top,
            right: filled_right.max(track.left + 2.0 * s),
            bottom: track.bottom,
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(filled),
            radiusX: 2.0 * s,
            radiusY: 2.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.accent_blue);
        let pointer_on_track = model
            .pointer
            .map(|(x, y)| layout.progress_hit(x, y))
            .unwrap_or(false);
        if pointer_on_track || model.drag_preview.is_some() {
            let knob = if model.drag_preview.is_some() {
                10.0 * s
            } else {
                8.0 * s
            };
            self.d2d.FillEllipse(
                &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                    point: Vector2::new(
                        filled_right.clamp(layout.track.left, layout.track.right),
                        layout.track.top + layout.track.height() * 0.5,
                    ),
                    radiusX: knob * 0.5,
                    radiusY: knob * 0.5,
                },
                &self.text,
            );
        }

        // Loop menu trigger wears the same chip as every other toolbar cell.
        let loop_hot = model
            .pointer
            .map(|(x, y)| layout.loop_btn.contains(x, y))
            .unwrap_or(false);
        let loop_active = model.loop_menu_open || model.loop_mode == o11a1::settings::LOOP_ONE;
        self.draw_toolbar_cell(layout.loop_btn, loop_active, loop_hot, s);
        self.icon_loop(
            (layout.loop_btn.left + layout.loop_btn.right) * 0.5,
            (layout.loop_btn.top + layout.loop_btn.bottom) * 0.5,
            s,
            loop_active,
        );

        // Divider rules between the three groups.
        self.fill(layout.divider1, &self.panel_border);
        self.fill(layout.divider2, &self.panel_border);

        // Storyboard / time bubble above the seek band. The time sits on an
        // opaque warm-paper pill so it stays legible over any video frame.
        if let Some(preview_ms) = model.drag_preview.or(model.hover_preview) {
            let fraction = if model.duration_ms > 0 {
                (preview_ms as f32 / model.duration_ms as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let anchor_x = layout.track.left + layout.track.width() * fraction;
            let bubble_w = 86.0 * s;
            let bubble = Rect {
                left: (anchor_x - bubble_w * 0.5)
                    .clamp(layout.track.left, layout.track.right - bubble_w),
                top: layout.panel.top - 36.0 * s,
                right: 0.0,
                bottom: layout.panel.top - 10.0 * s,
            };
            let bubble = Rect {
                right: bubble.left + bubble_w,
                ..bubble
            };
            let bubble_rounded = D2D1_ROUNDED_RECT {
                rect: to_d2d(bubble),
                radiusX: 7.0 * s,
                radiusY: 7.0 * s,
            };
            self.d2d
                .FillRoundedRectangle(&bubble_rounded, &self.stats_panel);
            self.d2d
                .DrawRoundedRectangle(&bubble_rounded, &self.panel_border, 1.0 * s, None);
            self.text_centered(
                &crate::format::format_time_ms(preview_ms),
                bubble,
                &self.body_format,
                &self.text,
            );
            if let Some(cell) = &model.storyboard_cell {
                let image_w = 176.0 * s;
                let image_h = 99.0 * s;
                let max_right = layout.top_bar.right.max(image_w + 8.0 * s);
                let left = (anchor_x - image_w * 0.5)
                    .clamp(8.0 * s, (max_right - image_w - 8.0 * s).max(8.0 * s));
                let top = (bubble.top - 6.0 * s - image_h).max(layout.top_bar.bottom + 4.0 * s);
                let frame = Rect {
                    left,
                    top,
                    right: left + image_w,
                    bottom: top + image_h,
                };
                if cell.image.width > 0 && cell.image.height > 0 && cell.time_ms == preview_ms {
                    let _ = self.draw_image(&cell.image, frame);
                    self.stroke_rect(frame, 1.0 * s, &self.panel_border);
                }
            }
        }

        self.draw_toolbar_buttons(model, layout);
        self.draw_toolbar_tooltip(model, layout);
    }

    /// One toolbar cell. Active toggles/tabs get only a soft blue wash; no
    /// outline, halo or underline. Hover adds a tactile outline; idle stays on
    /// the quiet chip.
    unsafe fn draw_toolbar_cell(&self, rect: Rect, active: bool, hot: bool, s: f32) {
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 10.0 * s,
            radiusY: 10.0 * s,
        };
        if active {
            self.d2d
                .FillRoundedRectangle(&rounded, &self.toolbar_active);
        } else if hot {
            self.d2d.FillRoundedRectangle(&rounded, &self.hover);
            self.d2d
                .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
        } else {
            self.d2d.FillRoundedRectangle(&rounded, &self.chip);
        }
    }

    unsafe fn draw_toolbar_buttons(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let buttons = layout.toolbar_buttons;
        for (index, rect) in buttons.iter().enumerate() {
            let hot = model
                .pointer
                .map(|(x, y)| rect.contains(x, y))
                .unwrap_or(false);
            let active = match index {
                TB_PRECISION => model.panel == 1 && model.precision_mix > 0.001,
                TB_SPEED => model.panel == 2 && model.precision_mix > 0.001,
                TB_MUTE => model.muted,
                TB_DANMAKU => model.danmaku_popup,
                TB_INFO => model.sidebar_open && model.sidebar_tab == SidebarTab::Info,
                TB_COMMENTS => model.sidebar_open && model.sidebar_tab == SidebarTab::Comments,
                TB_SERIES => model.sidebar_open && model.sidebar_tab == SidebarTab::Episodes,
                TB_PERF => model.sidebar_open && model.sidebar_tab == SidebarTab::Performance,
                TB_SETTINGS => model.sidebar_open && model.sidebar_tab == SidebarTab::Settings,
                _ => false,
            };
            self.draw_toolbar_cell(*rect, active, hot, s);
            let cx = (rect.left + rect.right) * 0.5;
            let cy = (rect.top + rect.bottom) * 0.5;
            let ink = if active { &self.accent } else { &self.text };
            match index {
                TB_PRECISION => self.icon_precision(cx, cy, s, active),
                TB_SPEED => self.icon_speed(cx, cy, s, active),
                TB_MUTE => self.icon_volume(cx, cy, s, model.muted),
                TB_DANMAKU => self.icon_danmaku(
                    cx,
                    cy,
                    s,
                    if active {
                        &self.accent
                    } else if model.danmaku_visible {
                        &self.text
                    } else {
                        &self.text_faint
                    },
                    model.danmaku_visible,
                ),
                TB_INFO => self.icon_info(cx, cy, s, ink),
                TB_COMMENTS => self.icon_comments(cx, cy, s, ink),
                TB_SERIES => self.icon_series(cx, cy, s, ink),
                TB_PERF => self.icon_perf(cx, cy, s, ink),
                TB_SETTINGS => self.icon_gear(cx, cy, s, ink),
                _ => {}
            }
        }
    }

    unsafe fn draw_toolbar_tooltip(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let Some((px, py)) = model.pointer else {
            return;
        };
        let mut anchor = None;
        let mut label: Option<String> = None;
        if layout.loop_btn.contains(px, py) {
            anchor = Some(layout.loop_btn);
            label = Some(
                match model.loop_mode {
                    o11a1::settings::LOOP_PAUSE => "结束行为：暂停",
                    o11a1::settings::LOOP_ONE => "结束行为：循环",
                    _ => "结束行为：继续",
                }
                .to_string(),
            );
        } else {
            for (index, rect) in layout.toolbar_buttons.iter().enumerate() {
                if !rect.contains(px, py) {
                    continue;
                }
                anchor = Some(*rect);
                label = Some(match index {
                    TB_PRECISION => {
                        if model.panel == 1 && model.precision_mix > 0.001 {
                            "返回工具栏".to_string()
                        } else {
                            "进阶进度条".to_string()
                        }
                    }
                    TB_SPEED => {
                        if model.panel == 2 && model.precision_mix > 0.001 {
                            "返回工具栏".to_string()
                        } else {
                            "播放速度".to_string()
                        }
                    }
                    TB_MUTE => {
                        if model.muted {
                            "取消静音".to_string()
                        } else {
                            "静音".to_string()
                        }
                    }
                    TB_DANMAKU => {
                        format!("弹幕：{}", if model.danmaku_visible { "开" } else { "关" })
                    }
                    TB_INFO => "视频信息".to_string(),
                    TB_COMMENTS => "评论".to_string(),
                    TB_SERIES => "选集".to_string(),
                    TB_PERF => "性能".to_string(),
                    TB_SETTINGS => "设置".to_string(),
                    _ => String::new(),
                });
                break;
            }
        }
        let (Some(anchor), Some(label)) = (anchor, label) else {
            return;
        };
        if label.is_empty() {
            return;
        }
        let width = (label.chars().count() as f32 * 11.0 + 26.0).max(48.0) * s;
        let height = 26.0 * s;
        let left = ((anchor.left + anchor.right) * 0.5 - width * 0.5).clamp(
            layout.stage.left + 8.0 * s,
            (layout.stage.right - width - 8.0 * s).max(8.0 * s),
        );
        let bottom = layout.panel.top - 8.0 * s;
        let rect = Rect {
            left,
            top: bottom - height,
            right: left + width,
            bottom,
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 6.0 * s,
            radiusY: 6.0 * s,
        };
        self.d2d
            .FillRoundedRectangle(&rounded, &self.sidebar_panel_bg);
        self.d2d
            .DrawRoundedRectangle(&rounded, &self.sidebar_panel_line, 1.0 * s, None);
        self.text_centered(&label, rect, &self.hint_format, &self.sidebar_text);
    }

    /// Wheel volume feedback: one solid vertical level bar at the picture's
    /// right edge. The bar itself is the whole control — no paper capsule, no
    /// white rim — just a track with the accent fill, like the seek band.
    unsafe fn draw_volume_osd(&self, model: &UiModel, layout: &Layout) {
        if !model.volume_wheel {
            return;
        }
        let s = layout.scale;
        let stage = layout.stage;
        let bar_w = 5.0 * s;
        let bar_h = (132.0 * s).min((stage.height() - 48.0 * s).max(72.0 * s));
        let right = stage.right - 18.0 * s;
        let left = right - bar_w;
        let top = (stage.top + stage.bottom - bar_h) * 0.5;
        let bar = Rect {
            left,
            top,
            right,
            bottom: top + bar_h,
        };
        let bar_round = D2D1_ROUNDED_RECT {
            rect: to_d2d(bar),
            radiusX: bar_w * 0.5,
            radiusY: bar_w * 0.5,
        };
        self.d2d.FillRoundedRectangle(&bar_round, &self.text_faint);

        let fraction = if model.muted {
            0.0
        } else {
            model.volume.min(100) as f32 / 100.0
        };
        if fraction > 0.002 {
            let fill_h = (bar.height() * fraction).max(bar_w);
            let fill = Rect {
                left: bar.left,
                top: bar.bottom - fill_h,
                right: bar.right,
                bottom: bar.bottom,
            };
            let fill_round = D2D1_ROUNDED_RECT {
                rect: to_d2d(fill),
                radiusX: bar_w * 0.5,
                radiusY: bar_w * 0.5,
            };
            self.d2d.FillRoundedRectangle(&fill_round, &self.accent_blue);
        }
    }

    unsafe fn draw_loop_menu(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let rect = layout.loop_menu;
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 10.0 * s,
            radiusY: 10.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.toast_bg);
        self.d2d
            .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
        let labels = ["循环", "继续", "下一分P", "暂停"];
        for (index, row) in layout.loop_rows.iter().enumerate() {
            let active = model.loop_mode == o11a1::settings::LOOP_MENU_ORDER[index];
            let hot = model
                .pointer
                .map(|(x, y)| row.contains(x, y))
                .unwrap_or(false);
            if active {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(*row),
                    radiusX: 7.0 * s,
                    radiusY: 7.0 * s,
                };
                self.d2d
                    .FillRoundedRectangle(&rounded, &self.toolbar_active);
                self.d2d
                    .DrawRoundedRectangle(&rounded, &self.toolbar_active_line, 1.0 * s, None);
            } else if hot {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(*row),
                    radiusX: 7.0 * s,
                    radiusY: 7.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.hover);
            }
            self.text_centered(
                labels[index],
                *row,
                &self.body_format,
                if active {
                    &self.text
                } else {
                    &self.text_dim
                },
            );
        }
    }

    unsafe fn draw_undo_toast(&mut self, undo: &UndoToastUi, layout: &Layout) {
        let s = layout.scale;
        let rect = layout.undo_toast;
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 8.0 * s,
            radiusY: 8.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.toast_bg);
        self.d2d
            .DrawRoundedRectangle(&rounded, &self.accent, 1.0 * s, None);
        self.text(
            &undo.text,
            rect.left + 12.0 * s,
            rect.top + 9.0 * s,
            (layout.undo_button.left - rect.left - 18.0 * s).max(60.0),
            &self.small_format,
            &self.text,
        );
        let button = layout.undo_button;
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(button),
            radiusX: 6.0 * s,
            radiusY: 6.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.accent);
        self.text_centered("撤销", button, &self.hint_format, &self.text);
    }

    unsafe fn draw_dm_menu(&mut self, model: &UiModel, menu: &DanmakuMenuUi, layout: &Layout) {
        let s = layout.scale;
        let panel = dm_menu_rect(menu.x, menu.y, layout.width, layout.height, s);
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(panel),
            radiusX: 8.0 * s,
            radiusY: 8.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.toast_bg);
        self.d2d
            .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
        for (index, label) in DM_MENU_LABELS.iter().enumerate() {
            let row = dm_menu_row(panel, index, s);
            let hot = model
                .pointer
                .map(|(x, y)| row.contains(x, y))
                .unwrap_or(false);
            if hot {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(row),
                    radiusX: 5.0 * s,
                    radiusY: 5.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.hover);
            }
            self.text(
                label,
                row.left + 10.0 * s,
                row.top + 8.0 * s,
                row.width() - 20.0 * s,
                &self.small_format,
                &self.text,
            );
        }
        self.fill(
            Rect {
                left: panel.left + 8.0 * s,
                top: dm_menu_row(panel, DM_MENU_REPORT, s).top - 2.0 * s,
                right: panel.right - 8.0 * s,
                bottom: dm_menu_row(panel, DM_MENU_REPORT, s).top - 1.0 * s,
            },
            &self.panel_border,
        );
        if menu.submenu {
            let report = dm_report_rect(panel, layout.width, layout.height, s);
            let rounded = D2D1_ROUNDED_RECT {
                rect: to_d2d(report),
                radiusX: 8.0 * s,
                radiusY: 8.0 * s,
            };
            self.d2d.FillRoundedRectangle(&rounded, &self.toast_bg);
            self.d2d
                .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
            for (index, label) in DM_REPORT_LABELS.iter().enumerate() {
                let row = dm_report_row(report, index, s);
                let hot = model
                    .pointer
                    .map(|(x, y)| row.contains(x, y))
                    .unwrap_or(false);
                if hot {
                    let rounded = D2D1_ROUNDED_RECT {
                        rect: to_d2d(row),
                        radiusX: 5.0 * s,
                        radiusY: 5.0 * s,
                    };
                    self.d2d.FillRoundedRectangle(&rounded, &self.hover);
                }
                let label = if index < DM_REPORT_LABELS.len() - 1 {
                    format!("{} {}", index + 1, label)
                } else {
                    "其他".to_string()
                };
                self.text(
                    &label,
                    row.left + 10.0 * s,
                    row.top + 8.0 * s,
                    row.width() - 20.0 * s,
                    &self.small_format,
                    &self.text,
                );
            }
        }
    }

    unsafe fn draw_danmaku_popup(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let popup = layout.popup;
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(popup),
            radiusX: 10.0 * s,
            radiusY: 10.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.toast_bg);
        self.d2d
            .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
        self.text(
            "弹幕设置",
            popup.left + 16.0 * s,
            popup.top + 12.0 * s,
            160.0 * s,
            &self.comment_name_format,
            &self.text,
        );

        // Row 0 follows the same label + control geometry as the rows below:
        // the label shares the left column and the switch sits in the right
        // column. The switch has no fill at all — only its border and text
        // colour change with the state.
        let toggle = layout.popup_toggle;
        let hot = model
            .pointer
            .map(|(x, y)| toggle.contains(x, y))
            .unwrap_or(false);
        self.popup_switch("弹幕", toggle, model.danmaku_visible, hot, layout);

        self.popup_slider(
            "透明度",
            layout.popup_opacity,
            model.danmaku_opacity as f32 / 100.0,
            &format!("{}%", model.danmaku_opacity),
            layout,
        );
        self.popup_slider(
            "字号",
            layout.popup_font,
            (model.danmaku_font as f32 - 50.0) / 100.0,
            &format!("{}%", model.danmaku_font),
            layout,
        );
        self.popup_slider(
            "速度",
            layout.popup_speed,
            (model.danmaku_speed as f32 - 50.0) / 150.0,
            &format!("{}%", model.danmaku_speed),
            layout,
        );
        self.popup_label("区域", layout.popup_area[0], layout);
        for (index, rect) in layout.popup_area.iter().enumerate() {
            let value = [25, 50, 75, 100][index];
            self.popup_choice(
                *rect,
                &format!("{value}%"),
                model.danmaku_area == value,
                layout,
            );
        }
        self.popup_label("屏蔽", layout.popup_blocks[0], layout);
        let block_labels = ["滚动", "顶部", "底部"];
        let block_active = [model.block_scroll, model.block_top, model.block_bottom];
        for (index, rect) in layout.popup_blocks.iter().enumerate() {
            self.popup_choice(*rect, block_labels[index], block_active[index], layout);
        }
        let density_hot = model
            .pointer
            .map(|(x, y)| layout.popup_density.contains(x, y))
            .unwrap_or(false);
        self.popup_switch(
            "分布密度",
            layout.popup_density,
            model.density_on,
            density_hot,
            layout,
        );
    }

    /// One popup boolean row: left label plus a right-aligned hollow pill.
    /// Only the border and the 开/关 ink change colour; the popup paper shows
    /// through, so the switch never reads as a filled banner.
    unsafe fn popup_switch(&self, label: &str, row: Rect, on: bool, hot: bool, layout: &Layout) {
        let s = layout.scale;
        self.popup_label(label, row, layout);
        let switch = Rect {
            left: layout.popup.right - 16.0 * s - 54.0 * s,
            top: (row.top + row.bottom) * 0.5 - 14.0 * s,
            right: layout.popup.right - 16.0 * s,
            bottom: (row.top + row.bottom) * 0.5 + 14.0 * s,
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(switch),
            radiusX: 14.0 * s,
            radiusY: 14.0 * s,
        };
        self.d2d.DrawRoundedRectangle(
            &rounded,
            if on || hot {
                &self.accent_blue
            } else {
                &self.text_faint
            },
            1.5 * s,
            None,
        );
        self.text_centered(
            if on { "开" } else { "关" },
            switch,
            &self.comment_body_format,
            &self.text,
        );
    }

    /// Uniform left-column label for a 40px popup row; `anchor` is any control
    /// in that row.
    unsafe fn popup_label(&self, label: &str, anchor: Rect, layout: &Layout) {
        let s = layout.scale;
        let mid = (anchor.top + anchor.bottom) * 0.5;
        self.text_box(
            label,
            Rect {
                left: layout.popup.left + 16.0 * s,
                top: mid - 20.0 * s,
                right: layout.popup.left + 96.0 * s,
                bottom: mid + 20.0 * s,
            },
            &self.comment_body_format,
            &self.text_dim,
            0,
        );
    }

    unsafe fn popup_slider(
        &self,
        label: &str,
        track: Rect,
        fraction: f32,
        value: &str,
        layout: &Layout,
    ) {
        let s = layout.scale;
        let mid = (track.top + track.bottom) * 0.5;
        self.text_box(
            label,
            Rect {
                left: layout.popup.left + 16.0 * s,
                top: mid - 20.0 * s,
                right: layout.popup.left + 96.0 * s,
                bottom: mid + 20.0 * s,
            },
            &self.comment_body_format,
            &self.text_dim,
            0,
        );
        let bar = D2D1_ROUNDED_RECT {
            rect: to_d2d(Rect {
                left: track.left,
                top: track.top - 2.0 * s,
                right: track.right,
                bottom: track.bottom + 2.0 * s,
            }),
            radiusX: 4.0 * s,
            radiusY: 4.0 * s,
        };
        self.d2d.FillRoundedRectangle(&bar, &self.track);
        let right = track.left + track.width() * fraction.clamp(0.0, 1.0);
        let filled = D2D1_ROUNDED_RECT {
            rect: to_d2d(Rect {
                left: track.left,
                top: track.top - 2.0 * s,
                right: right.max(track.left + 2.0 * s),
                bottom: track.bottom + 2.0 * s,
            }),
            radiusX: 4.0 * s,
            radiusY: 4.0 * s,
        };
        self.d2d.FillRoundedRectangle(&filled, &self.accent);
        self.d2d.FillEllipse(
            &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                point: Vector2::new(right, mid),
                radiusX: 5.0 * s,
                radiusY: 5.0 * s,
            },
            &self.text,
        );
        self.text_box(
            value,
            Rect {
                left: layout.popup.right - 64.0 * s,
                top: mid - 20.0 * s,
                right: layout.popup.right - 16.0 * s,
                bottom: mid + 20.0 * s,
            },
            &self.comment_body_format,
            &self.text_dim,
            2,
        );
    }

    unsafe fn popup_choice(&self, rect: Rect, label: &str, active: bool, layout: &Layout) {
        let s = layout.scale;
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 6.0 * s,
            radiusY: 6.0 * s,
        };
        self.d2d
            .FillRoundedRectangle(&rounded, if active { &self.accent } else { &self.chip });
        self.text_centered(
            label,
            rect,
            &self.comment_meta_format,
            if active {
                &self.accent_fg
            } else {
                &self.text_dim
            },
        );
    }

    unsafe fn line(
        &self,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        width: f32,
        brush: &ID2D1SolidColorBrush,
    ) {
        unsafe {
            let _ = self.render_target.DrawLine(
                Vector2::new(x1, y1),
                Vector2::new(x2, y2),
                brush,
                width,
                None,
            );
        }
    }

    unsafe fn stroke_rect(&self, rect: Rect, width: f32, brush: &ID2D1SolidColorBrush) {
        unsafe {
            let _ = self
                .render_target
                .DrawRectangle(&to_d2d(rect), brush, width, None);
        }
    }

    /// Crosshair for the precision-panel toggle: precise seek/selection.
    unsafe fn icon_precision(&self, cx: f32, cy: f32, s: f32, on: bool) {
        let brush = if on { &self.accent } else { &self.text_dim };
        self.icon_path(
            "M 8 2.8 A 5.2 5.2 0 1 1 7.99 2.8 \
             M 8 1.0 L 8 2.6 M 8 13.4 L 8 15.0 \
             M 1.0 8 L 2.6 8 M 13.4 8 L 15.0 8",
            16.0,
            16.0,
            cx,
            cy,
            s,
            brush,
            1.5 * s,
            false,
        );
    }

    unsafe fn icon_speed(&self, cx: f32, cy: f32, s: f32, on: bool) {
        let brush = if on { &self.accent } else { &self.text_dim };
        // Open-bottom dial with a needle and centre pin: the standard playback
        // speed metaphor, kept to three strokes so it stays crisp at 16px.
        self.icon_path(
            "M 3.0 12.0 A 6.2 6.2 0 1 1 13.0 12.0 M 8 9.0 L 10.6 5.9",
            16.0,
            16.0,
            cx,
            cy,
            s,
            brush,
            1.5 * s,
            false,
        );
        self.d2d.FillEllipse(
            &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                point: Vector2::new(cx, cy + 1.0 * s),
                radiusX: 1.25 * s,
                radiusY: 1.25 * s,
            },
            brush,
        );
    }

    unsafe fn icon_volume(&self, cx: f32, cy: f32, s: f32, muted: bool) {
        let ink = if muted { &self.text_dim } else { &self.text };
        self.icon_path(
            "M 1.75 6.2 L 4.55 6.2 L 7.75 3.2 L 7.75 12.8 L 4.55 9.8 L 1.75 9.8 Z",
            16.0,
            16.0,
            cx,
            cy,
            s,
            ink,
            1.5 * s,
            false,
        );
        if muted {
            self.icon_path(
                "M 9.95 6 L 13.35 9.4 M 13.35 6 L 9.95 9.4",
                16.0,
                16.0,
                cx,
                cy,
                s,
                ink,
                1.5 * s,
                false,
            );
        } else {
            self.icon_path(
                "M 9.75 5.9 C 10.95 6.8 10.95 9.2 9.75 10.1 M 11.95 3.6 C 14.25 5.4 14.25 10.6 11.95 12.4",
                16.0, 16.0, cx, cy, s, ink, 1.5 * s, false,
            );
        }
    }

    unsafe fn icon_loop(&self, cx: f32, cy: f32, s: f32, on: bool) {
        let brush = if on { &self.accent } else { &self.text_dim };
        // Two chasing arrows with rounded corners: a repeat/loop glyph, not a
        // refresh glyph. The arrowheads point along the cycle.
        self.icon_path(
            "M 3.2 6.4 A 2.8 2.8 0 0 1 6 3.6 L 12.2 3.6 \
             M 10.4 1.8 L 12.6 3.6 L 10.4 5.4 \
             M 12.8 9.6 A 2.8 2.8 0 0 1 10 12.4 L 3.8 12.4 \
             M 5.6 10.6 L 3.4 12.4 L 5.6 14.2",
            16.0,
            16.0,
            cx,
            cy,
            s,
            brush,
            1.5 * s,
            false,
        );
    }

    unsafe fn icon_interp(&self, cx: f32, cy: f32, s: f32, on: bool) {
        let brush = if on { &self.accent } else { &self.text_dim };
        self.icon_path(
            "M 2.2 5.2 L 9.2 5.2 L 9.2 12.2 L 2.2 12.2 Z M 5.6 2.4 L 13.8 2.4 L 13.8 10.6 L 5.6 10.6 Z",
            16.0, 16.0, cx, cy, s, brush, 1.5 * s, false,
        );
        self.icon_path(
            "M 7.2 7.2 L 11.1 9.2 L 7.2 11.2 Z",
            16.0,
            16.0,
            cx,
            cy,
            s,
            brush,
            1.2 * s,
            false,
        );
        if on {
            self.fill(
                Rect {
                    left: cx + 8.0 * s - 11.0 * s,
                    top: cy - 8.0 * s + 7.0 * s,
                    right: cx + 8.0 * s - 7.0 * s,
                    bottom: cy - 8.0 * s + 11.0 * s,
                },
                &self.accent,
            );
        }
    }

    unsafe fn icon_enhance(&self, cx: f32, cy: f32, s: f32, on: bool) {
        let brush = if on { &self.accent } else { &self.text_dim };
        self.icon_path(
            "M 10.3 1.4 L 11.2 4.2 L 14 5.1 L 11.2 6 L 10.3 8.8 L 9.4 6 L 6.6 5.1 L 9.4 4.2 Z",
            16.0,
            16.0,
            cx,
            cy,
            s,
            brush,
            1.5 * s,
            false,
        );
        self.icon_path(
            "M 4.4 8.2 L 5 9.7 L 6.5 10.3 L 5 10.9 L 4.4 12.4 L 3.8 10.9 L 2.3 10.3 L 3.8 9.7 Z",
            16.0,
            16.0,
            cx,
            cy,
            s,
            brush,
            1.5 * s,
            false,
        );
    }

    unsafe fn icon_info(&self, cx: f32, cy: f32, s: f32, b: &ID2D1SolidColorBrush) {
        self.icon_path(
            "M 8 1.9 A 6.1 6.1 0 0 1 8 14.1 A 6.1 6.1 0 0 1 8 1.9",
            16.0,
            16.0,
            cx,
            cy,
            s,
            b,
            1.5 * s,
            false,
        );
        let top = cy - 8.0 * s;
        self.fill(
            Rect {
                left: cx - 1.1 * s,
                top: top + 4.5 * s,
                right: cx + 1.1 * s,
                bottom: top + 6.7 * s,
            },
            b,
        );
        self.fill(
            Rect {
                left: cx - 1.1 * s,
                top: top + 8.1 * s,
                right: cx + 1.1 * s,
                bottom: top + 12.3 * s,
            },
            b,
        );
    }

    unsafe fn icon_comments(&self, cx: f32, cy: f32, s: f32, b: &ID2D1SolidColorBrush) {
        self.icon_path(
            "M 2.5 0.75 L 12.5 0.75 C 13.9 0.75 15 1.85 15 3.25 L 15 7.75 C 15 9.15 13.9 10.25 12.5 10.25 L 6 10.25 L 3.2 12.6 L 3.2 10.25 L 2.5 10.25 C 1.1 10.25 0 9.15 0 7.75 L 0 3.25 C 0 1.85 1.1 0.75 2.5 0.75 Z",
            15.0, 13.0, cx, cy, s, b, 1.5 * s, false,
        );
    }

    unsafe fn icon_series(&self, cx: f32, cy: f32, s: f32, b: &ID2D1SolidColorBrush) {
        self.icon_path(
            "M 2.2 3.1 L 4.4 3.1 L 4.4 5.1 L 2.2 5.1 Z M 6.6 4.1 L 13.8 4.1 M 2.2 7 L 4.4 7 L 4.4 9 L 2.2 9 Z M 6.6 8 L 13.8 8 M 2.2 10.9 L 4.4 10.9 L 4.4 12.9 L 2.2 12.9 Z M 6.6 11.9 L 13.8 11.9",
            16.0, 16.0, cx, cy, s, b, 1.5 * s, false,
        );
    }

    unsafe fn icon_perf(&self, cx: f32, cy: f32, s: f32, b: &ID2D1SolidColorBrush) {
        self.icon_path(
            "M 1.8 13.6 L 14.2 13.6 M 4.4 13.6 L 4.4 9.2 M 8 13.6 L 8 6.2 M 11.6 13.6 L 11.6 3.2",
            16.0,
            16.0,
            cx,
            cy,
            s,
            b,
            1.5 * s,
            false,
        );
    }

    unsafe fn icon_danmaku(
        &self,
        cx: f32,
        cy: f32,
        s: f32,
        brush: &ID2D1SolidColorBrush,
        visible: bool,
    ) {
        self.icon_path(
            "M 2 4.4 L 10.6 4.4 M 5.4 8 L 14 8 M 2.4 11.6 L 9.4 11.6",
            16.0,
            16.0,
            cx,
            cy,
            s,
            brush,
            1.5 * s,
            false,
        );
        if !visible {
            self.icon_path(
                "M 2.6 13 L 13.4 3",
                16.0,
                16.0,
                cx,
                cy,
                s,
                &self.danger_soft,
                1.5 * s,
                false,
            );
        }
    }

    unsafe fn icon_gear(&self, cx: f32, cy: f32, s: f32, brush: &ID2D1SolidColorBrush) {
        self.icon_path(
            "M 8 3.4 A 4.6 4.6 0 0 1 8 12.6 A 4.6 4.6 0 0 1 8 3.4",
            16.0,
            16.0,
            cx,
            cy,
            s,
            brush,
            1.5 * s,
            false,
        );
        self.icon_path("M 13.2 8 L 14.8 8 M 11.68 11.68 L 12.81 12.81 M 8 13.2 L 8 14.8 M 4.32 11.68 L 3.19 12.81 M 2.8 8 L 1.2 8 M 4.32 4.32 L 3.19 3.19 M 8 2.8 L 8 1.2 M 11.68 4.32 L 12.81 3.19", 16.0, 16.0, cx, cy, s, brush, 1.5 * s, false);
        self.icon_path(
            "M 8 6.5 A 1.5 1.5 0 0 1 8 9.5 A 1.5 1.5 0 0 1 8 6.5",
            16.0,
            16.0,
            cx,
            cy,
            s,
            brush,
            1.5 * s,
            false,
        );
    }

    unsafe fn icon_aspect(&self, cx: f32, cy: f32, s: f32) {
        let b = &self.text_dim;
        let len = 5.0 * s;
        for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            let x = cx + sx * 9.0 * s;
            let y = cy + sy * 7.0 * s;
            self.line(x, y, x - sx * len, y, 1.3 * s, b);
            self.line(x, y, x, y - sy * len, 1.3 * s, b);
        }
    }

    unsafe fn icon_camera(&self, cx: f32, cy: f32, s: f32) {
        let b = &self.text_dim;
        self.stroke_rect(
            Rect {
                left: cx - 9.0 * s,
                top: cy - 5.0 * s,
                right: cx + 9.0 * s,
                bottom: cy + 6.0 * s,
            },
            1.3 * s,
            b,
        );
        self.fill(
            Rect {
                left: cx - 4.0 * s,
                top: cy - 8.0 * s,
                right: cx + 4.0 * s,
                bottom: cy - 5.0 * s,
            },
            b,
        );
        unsafe {
            let _ = self.render_target.DrawEllipse(
                &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                    point: Vector2::new(cx, cy + 1.0 * s),
                    radiusX: 4.0 * s,
                    radiusY: 4.0 * s,
                },
                b,
                1.3 * s,
                None,
            );
        }
    }

    unsafe fn icon_mini(&self, cx: f32, cy: f32, s: f32) {
        let b = &self.text_dim;
        self.stroke_rect(
            Rect {
                left: cx - 9.0 * s,
                top: cy - 7.0 * s,
                right: cx + 9.0 * s,
                bottom: cy + 7.0 * s,
            },
            1.2 * s,
            b,
        );
        self.fill(
            Rect {
                left: cx + 1.0 * s,
                top: cy + 1.0 * s,
                right: cx + 9.0 * s,
                bottom: cy + 7.0 * s,
            },
            &self.chrome_soft,
        );
    }

    unsafe fn icon_fullscreen(&self, cx: f32, cy: f32, s: f32) {
        let b = &self.text_dim;
        let len = 5.0 * s;
        for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            let x = cx + sx * 9.0 * s;
            let y = cy + sy * 7.0 * s;
            self.line(x, y, x - sx * len, y, 1.3 * s, b);
            self.line(x, y, x, y - sy * len, 1.3 * s, b);
        }
    }

    unsafe fn icon_playlist(&self, cx: f32, cy: f32, s: f32) {
        let b = &self.text_dim;
        for row in 0..3 {
            self.line(
                cx - 9.0 * s,
                cy - 5.0 * s + row as f32 * 5.0 * s,
                cx + 5.0 * s,
                cy - 5.0 * s + row as f32 * 5.0 * s,
                1.2 * s,
                b,
            );
        }
        self.fill(
            Rect {
                left: cx + 5.0 * s,
                top: cy - 1.0 * s,
                right: cx + 10.0 * s,
                bottom: cy + 7.0 * s,
            },
            b,
        );
    }

    unsafe fn icon_sidebar(&self, cx: f32, cy: f32, s: f32, brush: &ID2D1SolidColorBrush) {
        self.stroke_rect(
            Rect {
                left: cx - 9.0 * s,
                top: cy - 7.0 * s,
                right: cx + 9.0 * s,
                bottom: cy + 7.0 * s,
            },
            1.2 * s,
            brush,
        );
        self.line(
            cx + 2.0 * s,
            cy - 7.0 * s,
            cx + 2.0 * s,
            cy + 7.0 * s,
            1.2 * s,
            brush,
        );
    }

    fn text_wrapped(
        &self,
        text: &str,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        format: &IDWriteTextFormat,
        brush: &ID2D1SolidColorBrush,
    ) {
        if !self.text_ok {
            return;
        }
        if let Some(style) = self
            .styles
            .get(&(windows::core::Interface::as_raw(format) as usize))
        {
            if let Some(gdi) = self.gdi.borrow_mut().as_mut() {
                unsafe {
                    gdi.draw(
                        &self.render_target,
                        text,
                        *style,
                        x,
                        y,
                        width,
                        height,
                        0,
                        false,
                        true,
                        brush_rgba(brush),
                    );
                }
                return;
            }
        }
        if let Some(layout) = make_layout(&self.dwrite, format, text, width, height) {
            let y = snap_layout_y(&layout, y);
            unsafe {
                self.d2d.DrawTextLayout(
                    Vector2::new(x.round(), y),
                    &layout,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
        }
    }

    unsafe fn draw_image(&mut self, data: &ImageData, dest: Rect) -> bool {
        let Some(bitmap) = self.bitmap_for(data) else {
            return false;
        };
        unsafe {
            let rect = to_d2d(dest);
            let _ = self.render_target.DrawBitmap(
                &bitmap,
                Some(&rect),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
        }
        true
    }

    /// A copy glyph: two offset outlined cards, drawn with the same hairline
    /// weight as the other quiet icons.
    unsafe fn draw_copy_icon(
        &self,
        rect: Rect,
        s: f32,
        brush: &ID2D1SolidColorBrush,
    ) {
        let w = 9.0 * s;
        let h = 10.0 * s;
        let centre_x = (rect.left + rect.right) * 0.5;
        let centre_y = (rect.top + rect.bottom) * 0.5;
        let back = Rect {
            left: centre_x - w * 0.10 + w * 0.32,
            top: centre_y - h * 0.58,
            right: centre_x - w * 0.10 + w * 1.32,
            bottom: centre_y - h * 0.58 + h,
        };
        let front = Rect {
            left: centre_x - w * 0.58,
            top: centre_y - h * 0.22,
            right: centre_x - w * 0.58 + w,
            bottom: centre_y - h * 0.22 + h,
        };
        for shape in [back, front] {
            let rounded = D2D1_ROUNDED_RECT {
                rect: to_d2d(shape),
                radiusX: 2.0 * s,
                radiusY: 2.0 * s,
            };
            self.d2d
                .DrawRoundedRectangle(&rounded, brush, 1.2 * s, None);
        }
    }

    /// Cover image clipped to a rounded rectangle. Falls back to a plain
    /// draw if the driver rejects the layer, so the cover never disappears.
    unsafe fn draw_image_rounded(&mut self, data: &ImageData, dest: Rect, radius: f32) -> bool {
        let Some(bitmap) = self.bitmap_for(data) else {
            return false;
        };
        // Fill the banner without distorting the thumbnail: crop the source
        // to the same aspect as the destination before the rounded mask.
        let source = cover_source_rect(data, dest);
        let source_rect = D2D_RECT_F {
            left: source.left,
            top: source.top,
            right: source.right,
            bottom: source.bottom,
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(dest),
            radiusX: radius,
            radiusY: radius,
        };
        let geometry = match unsafe { self._d2d_factory.CreateRoundedRectangleGeometry(&rounded) } {
            Ok(geometry) => geometry,
            Err(_) => return self.draw_image(data, dest),
        };
        let mask: windows::Win32::Graphics::Direct2D::ID2D1Geometry =
            match geometry.cast() {
                Ok(mask) => mask,
                Err(_) => return self.draw_image(data, dest),
            };
        let layer = match unsafe { self.d2d.CreateLayer(None) } {
            Ok(layer) => layer,
            Err(_) => return self.draw_image(data, dest),
        };
        let params = windows::Win32::Graphics::Direct2D::D2D1_LAYER_PARAMETERS1 {
            contentBounds: to_d2d(dest),
            geometricMask: ManuallyDrop::new(Some(mask)),
            maskAntialiasMode:
                windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: 1.0,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: windows::Win32::Graphics::Direct2D::D2D1_LAYER_OPTIONS1_NONE,
        };
        unsafe { self.d2d.PushLayer(&params, &layer) };
        let rect = to_d2d(dest);
        let _ = self.render_target.DrawBitmap(
            &bitmap,
            Some(&rect),
            1.0,
            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            Some(&source_rect),
        );
        unsafe { self.d2d.PopLayer() };
        true
    }


    /// Draw an avatar clipped to its circle. D2D's geometric-mask layer is the
    /// only cheap path here; if the driver rejects the layer the image is drawn
    /// inset inside the already-painted circle instead of overflowing it.
    unsafe fn draw_image_circular(&mut self, data: &ImageData, dest: Rect) -> bool {
        let Some(bitmap) = self.bitmap_for(data) else {
            return false;
        };
        let center = Vector2::new(
            (dest.left + dest.right) * 0.5,
            (dest.top + dest.bottom) * 0.5,
        );
        let ellipse = windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
            point: center,
            radiusX: dest.width() * 0.5,
            radiusY: dest.height() * 0.5,
        };
        let geometry = match unsafe { self._d2d_factory.CreateEllipseGeometry(&ellipse) } {
            Ok(geometry) => geometry,
            Err(_) => return self.draw_image_contained(&bitmap, dest, 3.0 * self.layout.scale),
        };
        let mask: windows::Win32::Graphics::Direct2D::ID2D1Geometry = match geometry.cast() {
            Ok(mask) => mask,
            Err(_) => return self.draw_image_contained(&bitmap, dest, 3.0 * self.layout.scale),
        };
        let layer = match unsafe { self.d2d.CreateLayer(None) } {
            Ok(layer) => layer,
            Err(_) => return self.draw_image_contained(&bitmap, dest, 3.0 * self.layout.scale),
        };
        let params = windows::Win32::Graphics::Direct2D::D2D1_LAYER_PARAMETERS1 {
            contentBounds: to_d2d(dest),
            geometricMask: ManuallyDrop::new(Some(mask)),
            maskAntialiasMode:
                windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: 1.0,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: windows::Win32::Graphics::Direct2D::D2D1_LAYER_OPTIONS1_NONE,
        };
        unsafe { self.d2d.PushLayer(&params, &layer) };
        unsafe {
            let rect = to_d2d(dest);
            let _ = self.render_target.DrawBitmap(
                &bitmap,
                Some(&rect),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
            self.d2d.PopLayer();
        }
        true
    }

    unsafe fn draw_image_contained(&self, bitmap: &ID2D1Bitmap1, dest: Rect, inset: f32) -> bool {
        let contained = Rect {
            left: dest.left + inset,
            top: dest.top + inset,
            right: dest.right - inset,
            bottom: dest.bottom - inset,
        };
        unsafe {
            let rect = to_d2d(contained);
            let _ = self.render_target.DrawBitmap(
                bitmap,
                Some(&rect),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
        }
        true
    }

    unsafe fn bitmap_for(&mut self, data: &ImageData) -> Option<ID2D1Bitmap1> {
        if let Some(bitmap) = self.bitmaps.get(&data.id) {
            return Some(bitmap.clone());
        }
        if data.width == 0 || data.height == 0 {
            return None;
        }
        let mut bgra = Vec::with_capacity(data.rgba.len());
        for pixel in data.rgba.chunks_exact(4) {
            bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: ManuallyDrop::new(None),
        };
        let bitmap = unsafe {
            self.d2d
                .CreateBitmap(
                    D2D_SIZE_U {
                        width: data.width,
                        height: data.height,
                    },
                    Some(bgra.as_ptr() as *const _),
                    data.width * 4,
                    &properties as *const _,
                )
                .ok()?
        };
        if self.bitmaps.len() > 64 {
            self.bitmaps.clear();
        }
        self.bitmaps.insert(data.id, bitmap.clone());
        Some(bitmap)
    }

    unsafe fn icon_path(
        &self,
        commands: &str,
        view_w: f32,
        view_h: f32,
        cx: f32,
        cy: f32,
        s: f32,
        brush: &ID2D1SolidColorBrush,
        stroke: f32,
        filled: bool,
    ) {
        let dest = Rect {
            left: cx - view_w * 0.5 * s,
            top: cy - view_h * 0.5 * s,
            right: cx + view_w * 0.5 * s,
            bottom: cy + view_h * 0.5 * s,
        };
        let Some(geometry) = (unsafe { self.build_icon_geometry(commands, view_w, view_h, dest) })
        else {
            return;
        };
        let Ok(geometry) = geometry.cast::<ID2D1Geometry>() else {
            return;
        };
        let Ok(brush_any) = brush.cast::<ID2D1Brush>() else {
            return;
        };
        unsafe {
            if filled {
                let _ = self
                    .render_target
                    .FillGeometry(&geometry, &brush_any, None::<&ID2D1Brush>);
            } else {
                let _ = self.render_target.DrawGeometry(
                    &geometry,
                    &brush_any,
                    stroke,
                    Some(&self.icon_stroke),
                );
            }
        }
    }

    unsafe fn build_icon_geometry(
        &self,
        commands: &str,
        view_w: f32,
        view_h: f32,
        dest: Rect,
    ) -> Option<ID2D1PathGeometry1> {
        #[derive(Clone, Copy)]
        enum Token {
            Cmd(char),
            Num(f32),
        }
        let mut tokens = Vec::new();
        let mut chars = commands.chars().peekable();
        while let Some(&ch) = chars.peek() {
            if ch.is_ascii_alphabetic() {
                tokens.push(Token::Cmd(ch));
                chars.next();
            } else if ch == ',' || ch.is_whitespace() {
                chars.next();
            } else {
                let mut number = String::new();
                while let Some(&next) = chars.peek() {
                    if next == ',' || next.is_whitespace() || next.is_ascii_alphabetic() {
                        break;
                    }
                    number.push(next);
                    chars.next();
                }
                if let Ok(value) = number.parse::<f32>() {
                    tokens.push(Token::Num(value));
                }
            }
        }
        let geometry = unsafe { self._d2d_factory.CreatePathGeometry().ok()? };
        let sink = unsafe { geometry.Open().ok()? };
        let map = |x: f32, y: f32| {
            Vector2::new(
                dest.left + x / view_w * dest.width(),
                dest.top + y / view_h * dest.height(),
            )
        };
        let mut index = 0usize;
        let mut command = ' ';
        let mut current = Vector2::new(0.0, 0.0);
        let mut start = current;
        let mut figure_open = false;
        while index < tokens.len() {
            if let Token::Cmd(next) = tokens[index] {
                command = next;
                index += 1;
                if command == 'Z' {
                    if figure_open {
                        unsafe { sink.EndFigure(D2D1_FIGURE_END_CLOSED) };
                        figure_open = false;
                    }
                    current = start;
                }
                continue;
            }
            let args = match command {
                'M' | 'L' => 2,
                'C' => 6,
                'A' => 7,
                _ => break,
            };
            if index + args > tokens.len() {
                break;
            }
            let mut nums = [0.0f32; 7];
            for slot in nums.iter_mut().take(args) {
                match tokens[index] {
                    Token::Num(value) => *slot = value,
                    Token::Cmd(_) => break,
                }
                index += 1;
            }
            match command {
                'M' => {
                    if figure_open {
                        unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
                    }
                    current = map(nums[0], nums[1]);
                    start = current;
                    unsafe { sink.BeginFigure(current, D2D1_FIGURE_BEGIN_FILLED) };
                    figure_open = true;
                    command = 'L';
                }
                'L' => {
                    if !figure_open {
                        unsafe { sink.BeginFigure(current, D2D1_FIGURE_BEGIN_FILLED) };
                        figure_open = true;
                    }
                    let point = map(nums[0], nums[1]);
                    unsafe { sink.AddLine(point) };
                    current = point;
                }
                'C' => {
                    if !figure_open {
                        unsafe { sink.BeginFigure(current, D2D1_FIGURE_BEGIN_FILLED) };
                        figure_open = true;
                    }
                    let segment = D2D1_BEZIER_SEGMENT {
                        point1: map(nums[0], nums[1]),
                        point2: map(nums[2], nums[3]),
                        point3: map(nums[4], nums[5]),
                    };
                    unsafe { sink.AddBezier(&segment) };
                    current = segment.point3;
                }
                'A' => {
                    if !figure_open {
                        unsafe { sink.BeginFigure(current, D2D1_FIGURE_BEGIN_FILLED) };
                        figure_open = true;
                    }
                    let point = map(nums[5], nums[6]);
                    let segment = D2D1_ARC_SEGMENT {
                        point,
                        size: D2D_SIZE_F {
                            width: (nums[0] / view_w * dest.width()).abs(),
                            height: (nums[1] / view_h * dest.height()).abs(),
                        },
                        rotationAngle: nums[2],
                        sweepDirection: if nums[4] != 0.0 {
                            D2D1_SWEEP_DIRECTION_CLOCKWISE
                        } else {
                            D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
                        },
                        arcSize: if nums[3] != 0.0 {
                            D2D1_ARC_SIZE_LARGE
                        } else {
                            D2D1_ARC_SIZE_SMALL
                        },
                    };
                    unsafe { sink.AddArc(&segment) };
                    current = point;
                }
                _ => break,
            }
        }
        if figure_open {
            unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
        }
        unsafe { sink.Close().ok()? };
        Some(geometry)
    }

    fn fill(&self, rect: Rect, brush: &ID2D1SolidColorBrush) {
        unsafe {
            self.render_target.FillRectangle(&to_d2d(rect), brush);
        }
    }

    fn fill_brush(&self, rect: Rect, brush: &ID2D1Brush) {
        unsafe {
            self.render_target.FillRectangle(&to_d2d(rect), brush);
        }
    }

    /// Paint the opaque plate outside the picture. The overlay sits above the
    /// video, so only the letterbox / pillarbox axis is filled; the picture
    /// itself stays edge-to-edge and gets no outline.
    unsafe fn fill_stage_plate(
        &self,
        layout: &Layout,
        video: Rect,
        radius: f32,
        brush: &ID2D1SolidColorBrush,
        light: bool,
    ) {
        let stage_w = layout.stage.right;
        let height = layout.height;
        // The letterbox / pillarbox bands use the same home wallpaper as the
        // lane, so the picture sits directly in one continuous paper surface.
        if video.top > 0.0 {
            self.draw_home_backdrop(
                Rect {
                    left: 0.0,
                    top: 0.0,
                    right: stage_w,
                    bottom: video.top.min(height),
                },
                layout.stage,
                light,
            );
        }
        if video.bottom < height {
            self.draw_home_backdrop(
                Rect {
                    left: 0.0,
                    top: video.bottom.max(0.0),
                    right: stage_w,
                    bottom: height,
                },
                layout.stage,
                light,
            );
        }
        if video.left > 0.0 {
            self.draw_home_backdrop(
                Rect {
                    left: 0.0,
                    top: video.top.max(0.0),
                    right: video.left.min(stage_w),
                    bottom: video.bottom.min(height),
                },
                layout.stage,
                light,
            );
        }
        if video.right < stage_w {
            self.draw_home_backdrop(
                Rect {
                    left: video.right.max(0.0),
                    top: video.top.max(0.0),
                    right: stage_w,
                    bottom: video.bottom.min(height),
                },
                layout.stage,
                light,
            );
        }

        let r = radius
            .min(video.width() * 0.5)
            .min(video.height() * 0.5)
            .max(0.0);
        if r > 0.5 {
            self.fill_video_corner(0, video, r, brush);
            self.fill_video_corner(1, video, r, brush);
            self.fill_video_corner(2, video, r, brush);
            self.fill_video_corner(3, video, r, brush);
        }
    }

    /// Cover one corner square outside the rounded picture with the plate
    /// colour. The four masks are what make the video itself look rounded.
    fn fill_video_corner(&self, corner: u8, v: Rect, r: f32, brush: &ID2D1SolidColorBrush) {
        let (outer, start, end, sweep) = match corner {
            0 => (
                Vector2::new(v.left, v.top),
                Vector2::new(v.left + r, v.top),
                Vector2::new(v.left, v.top + r),
                D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE,
            ),
            1 => (
                Vector2::new(v.right, v.top),
                Vector2::new(v.right - r, v.top),
                Vector2::new(v.right, v.top + r),
                D2D1_SWEEP_DIRECTION_CLOCKWISE,
            ),
            2 => (
                Vector2::new(v.right, v.bottom),
                Vector2::new(v.right, v.bottom - r),
                Vector2::new(v.right - r, v.bottom),
                D2D1_SWEEP_DIRECTION_CLOCKWISE,
            ),
            _ => (
                Vector2::new(v.left, v.bottom),
                Vector2::new(v.left + r, v.bottom),
                Vector2::new(v.left, v.bottom - r),
                D2D1_SWEEP_DIRECTION_CLOCKWISE,
            ),
        };
        unsafe {
            let geometry: ID2D1PathGeometry1 = match self._d2d_factory.CreatePathGeometry() {
                Ok(geometry) => geometry,
                Err(_) => return,
            };
            let sink = match geometry.Open() {
                Ok(sink) => sink,
                Err(_) => return,
            };
            sink.BeginFigure(outer, D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLine(start);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: end,
                size: D2D_SIZE_F {
                    width: r,
                    height: r,
                },
                rotationAngle: 0.0,
                sweepDirection: sweep,
                arcSize: D2D1_ARC_SIZE_SMALL,
            });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            let _ = sink.Close();
            self.d2d.FillGeometry(&geometry, brush, None);
        }
    }

    /// Width of one line in the given face, using the same DirectWrite
    /// metrics that `text_right` uses. The GDI rasteriser follows those metrics
    /// closely enough for the precision header's sequential spacing.
    fn text_width(&self, text: &str, format: &IDWriteTextFormat) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        if let Some(layout) = make_layout(&self.dwrite, format, text, 4096.0, 64.0) {
            let mut metrics = DWRITE_TEXT_METRICS::default();
            unsafe {
                if layout.GetMetrics(&mut metrics).is_ok() {
                    return metrics.width;
                }
            }
        }
        text.chars().count() as f32 * 8.0
    }

    /// Draw one line with its baseline exactly on `baseline`, so the mixed
    /// sizes in the precision header share one optical bottom edge.
    fn text_on_baseline(
        &self,
        text: &str,
        x: f32,
        baseline: f32,
        max_width: f32,
        format: &IDWriteTextFormat,
        brush: &ID2D1SolidColorBrush,
    ) {
        let offset = self
            .styles
            .get(&(windows::core::Interface::as_raw(format) as usize))
            .map(|style| style.baseline)
            .unwrap_or(12.0);
        self.text(text, x, baseline - offset, max_width, format, brush);
    }

    #[allow(clippy::too_many_arguments)]
    fn text(
        &self,
        text: &str,
        x: f32,
        y: f32,
        max_width: f32,
        format: &IDWriteTextFormat,
        brush: &ID2D1SolidColorBrush,
    ) {
        if !self.text_ok {
            return;
        }
        if let Some(style) = self
            .styles
            .get(&(windows::core::Interface::as_raw(format) as usize))
        {
            if let Some(gdi) = self.gdi.borrow_mut().as_mut() {
                unsafe {
                    gdi.draw(
                        &self.render_target,
                        text,
                        *style,
                        x,
                        y,
                        max_width,
                        42.0,
                        0,
                        false,
                        false,
                        brush_rgba(brush),
                    );
                }
                return;
            }
        }
        if let Some(layout) = make_layout(&self.dwrite, format, text, max_width, 42.0) {
            let y = snap_layout_y(&layout, y);
            unsafe {
                self.d2d.DrawTextLayout(
                    Vector2::new(x.round(), y),
                    &layout,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
        }
    }

    /// Single-line text centred inside a box with explicit horizontal
    /// alignment (0 left, 1 centre, 2 right). It uses the same line metrics as
    /// `text_centered`, so row labels and their chips share one baseline.
    fn text_box(
        &self,
        text: &str,
        rect: Rect,
        format: &IDWriteTextFormat,
        brush: &ID2D1SolidColorBrush,
        align: u8,
    ) {
        if !self.text_ok {
            return;
        }
        if let Some(style) = self
            .styles
            .get(&(windows::core::Interface::as_raw(format) as usize))
        {
            if let Some(gdi) = self.gdi.borrow_mut().as_mut() {
                let offset = ((rect.height() - style.line_height) * 0.5).max(0.0);
                unsafe {
                    gdi.draw(
                        &self.render_target,
                        text,
                        *style,
                        rect.left,
                        rect.top + offset,
                        rect.width().max(20.0),
                        rect.height().max(16.0),
                        align,
                        false,
                        false,
                        brush_rgba(brush),
                    );
                }
                return;
            }
        }
        let line_height = self
            .styles
            .get(&(windows::core::Interface::as_raw(format) as usize))
            .map(|style| style.line_height)
            .unwrap_or(16.0);
        let y = rect.top + ((rect.height() - line_height).max(0.0)) * 0.5;
        if align == 2 {
            self.text_right(text, rect.right, y, rect.width(), format, brush);
        } else {
            self.text(text, rect.left, y, rect.width(), format, brush);
        }
    }

    fn text_centered(
        &self,
        text: &str,
        rect: Rect,
        format: &IDWriteTextFormat,
        brush: &ID2D1SolidColorBrush,
    ) {
        if !self.text_ok {
            return;
        }
        if let Some(style) = self
            .styles
            .get(&(windows::core::Interface::as_raw(format) as usize))
        {
            if let Some(gdi) = self.gdi.borrow_mut().as_mut() {
                let offset = ((rect.height() - style.line_height) * 0.5).max(0.0);
                unsafe {
                    gdi.draw(
                        &self.render_target,
                        text,
                        *style,
                        rect.left,
                        rect.top + offset,
                        rect.width().max(20.0),
                        rect.height().max(16.0),
                        1,
                        false,
                        false,
                        brush_rgba(brush),
                    );
                }
                return;
            }
        }
        if let Some(layout) = make_layout(
            &self.dwrite,
            format,
            text,
            rect.width().max(20.0),
            rect.height().max(16.0),
        ) {
            let line_height = self
                .styles
                .get(&(windows::core::Interface::as_raw(format) as usize))
                .map(|style| style.line_height)
                .unwrap_or(16.0);
            let y = snap_layout_y(
                &layout,
                rect.top + ((rect.height() - line_height).max(0.0)) * 0.5,
            );
            unsafe {
                self.d2d.DrawTextLayout(
                    Vector2::new(rect.left.round(), y),
                    &layout,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
        }
    }

    fn text_right(
        &self,
        text: &str,
        right: f32,
        y: f32,
        max_width: f32,
        format: &IDWriteTextFormat,
        brush: &ID2D1SolidColorBrush,
    ) {
        if !self.text_ok {
            return;
        }
        if let Some(style) = self
            .styles
            .get(&(windows::core::Interface::as_raw(format) as usize))
        {
            if let Some(gdi) = self.gdi.borrow_mut().as_mut() {
                unsafe {
                    gdi.draw(
                        &self.render_target,
                        text,
                        *style,
                        right - max_width,
                        y,
                        max_width,
                        30.0,
                        2,
                        false,
                        false,
                        brush_rgba(brush),
                    );
                }
                return;
            }
        }
        if let Some(layout) = make_layout(&self.dwrite, format, text, max_width, 30.0) {
            let mut metrics = DWRITE_TEXT_METRICS::default();
            let width = unsafe {
                layout
                    .GetMetrics(&mut metrics)
                    .map(|_| metrics.width)
                    .unwrap_or(max_width)
            };
            let y = snap_layout_y(&layout, y);
            unsafe {
                self.d2d.DrawTextLayout(
                    Vector2::new((right - width).round(), y),
                    &layout,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
        }
    }
}

/// Resolve the bundled rounded UI face privately. Chill Round Gothic (a
/// rounded Source Han Sans SC) covers the common simplified set and the rare
/// glyphs; YaHei is the system fallback when the files cannot be loaded.
pub(crate) fn player_font_family() -> PCWSTR {
    if chill_loaded() {
        w!("Chill Round Gothic")
    } else {
        w!("Microsoft YaHei UI")
    }
}

/// Danmaku uses the same family as the rest of the player, so comments and
/// controls share one voice. Kept separate from `player_font_family` so a
/// future design can give danmaku its own face without touching the UI.
pub(crate) fn danmaku_font_family() -> PCWSTR {
    if chill_loaded() {
        w!("Chill Round Gothic")
    } else {
        w!("Microsoft YaHei UI")
    }
}

pub(crate) fn player_font_available() -> bool {
    chill_loaded()
}

fn font_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("assets/fonts"));
            dirs.push(dir.join("../assets/fonts"));
        }
    }
    dirs.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fonts"));
    dirs.push(std::path::PathBuf::from("assets/fonts"));
    dirs
}

fn load_font_file(path: &std::path::Path) -> bool {
    if !path.exists() {
        return false;
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        AddFontResourceExW(
            PCWSTR(wide.as_ptr()),
            FONT_RESOURCE_CHARACTERISTICS(FR_NOT_ENUM.0),
            None,
        ) > 0
    }
}

/// Font files loaded into this process. They are deliberately not FR_PRIVATE:
/// DirectWrite's system collection cannot see private GDI resources, which is
/// what made the header and danmaku fall back to YaHei while the rest of the
/// chrome used the rounded face. FR_NOT_ENUM keeps them out of font pickers;
/// the paths are removed when the player process exits.
fn loaded_font_files() -> &'static OnceLock<Vec<std::path::PathBuf>> {
    static LOADED_FONTS: OnceLock<Vec<std::path::PathBuf>> = OnceLock::new();
    &LOADED_FONTS
}

fn chill_loaded() -> bool {
    static LOADED: OnceLock<bool> = OnceLock::new();
    *LOADED.get_or_init(|| {
        let files = [
            "ChillRoundGothic_Regular.ttf",
            "ChillRoundGothic_Medium.ttf",
            "ChillRoundGothic_Bold.ttf",
        ];
        for dir in font_dirs() {
            let mut paths = Vec::new();
            for file in files {
                let path = dir.join(file);
                if load_font_file(&path) {
                    paths.push(path);
                }
            }
            if !paths.is_empty() {
                let _ = loaded_font_files().set(paths);
                return true;
            }
        }
        false
    })
}

/// Drop the non-private font resources at process exit.
pub(crate) fn unload_player_fonts() {
    let Some(paths) = loaded_font_files().get() else {
        return;
    };
    for path in paths {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let _ = RemoveFontResourceExW(PCWSTR(wide.as_ptr()), FR_NOT_ENUM.0, None);
        }
    }
}

fn text_style(
    dwrite: &IDWriteFactory,
    format: &IDWriteTextFormat,
    size: f32,
    weight: u16,
) -> gdi_text::Style {
    let mut baseline = size * 0.88;
    let mut line_height = size * 1.2;
    let probe: Vec<u16> = "国".encode_utf16().collect();
    unsafe {
        if let Ok(layout) = dwrite.CreateTextLayout(&probe, format, 200.0, 200.0) {
            let mut metrics = [DWRITE_LINE_METRICS::default(); 1];
            let mut count = 0u32;
            let _ = layout.GetLineMetrics(Some(&mut metrics), &mut count);
            if count > 0 {
                baseline = metrics[0].baseline;
                line_height = metrics[0].height.max(1.0);
            }
        }
    }
    gdi_text::Style {
        size,
        weight,
        baseline,
        line_height,
    }
}

fn brush_rgba(brush: &ID2D1SolidColorBrush) -> [f32; 4] {
    let color = unsafe { brush.GetColor() };
    [color.r, color.g, color.b, color.a]
}

fn text_format(
    dwrite: &IDWriteFactory,
    size: f32,
    weight: DWRITE_FONT_WEIGHT,
) -> Result<IDWriteTextFormat> {
    // One rounded family everywhere, including small chrome text.
    let family = player_font_family();
    unsafe {
        let format = dwrite.CreateTextFormat(
            family,
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("zh-cn"),
        )?;
        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        Ok(format)
    }
}

/// Compact readout for the debug FPS and the frame ruler. It uses the same
/// rounded family as every other control so no monospace face leaks into the
/// toolbar or precision panel.
fn mono_format(dwrite: &IDWriteFactory, size: f32) -> Result<IDWriteTextFormat> {
    mono_format_weight(dwrite, size, DWRITE_FONT_WEIGHT_NORMAL)
}

fn mono_format_weight(
    dwrite: &IDWriteFactory,
    size: f32,
    weight: DWRITE_FONT_WEIGHT,
) -> Result<IDWriteTextFormat> {
    unsafe {
        let format = dwrite.CreateTextFormat(
            player_font_family(),
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("zh-cn"),
        )?;
        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        Ok(format)
    }
}

fn radial_brush(d2d: &ID2D1RenderTarget, color: D2D1_COLOR_F) -> Result<ID2D1RadialGradientBrush> {
    unsafe {
        let clear = D2D1_COLOR_F {
            r: color.r,
            g: color.g,
            b: color.b,
            a: 0.0,
        };
        let stops = [
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color,
            },
            D2D1_GRADIENT_STOP {
                position: 0.64,
                color: clear,
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: clear,
            },
        ];
        let collection =
            d2d.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
        let properties = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
            center: Vector2::new(0.0, 0.0),
            gradientOriginOffset: Vector2::new(0.0, 0.0),
            radiusX: 1.0,
            radiusY: 1.0,
        };
        Ok(d2d.CreateRadialGradientBrush(&properties, None, &collection)?)
    }
}

fn dot_brush(
    d2d: &ID2D1RenderTarget,
    red: u8,
    green: u8,
    blue: u8,
    alpha: u8,
) -> Result<ID2D1BitmapBrush> {
    unsafe {
        let mut pixels = [0u8; 3 * 3 * 4];
        pixels[0] = ((blue as u16 * alpha as u16) / 255) as u8;
        pixels[1] = ((green as u16 * alpha as u16) / 255) as u8;
        pixels[2] = ((red as u16 * alpha as u16) / 255) as u8;
        pixels[3] = alpha;
        let properties = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        let bitmap: ID2D1Bitmap = d2d.CreateBitmap(
            D2D_SIZE_U {
                width: 3,
                height: 3,
            },
            Some(pixels.as_ptr() as *const _),
            12,
            &properties,
        )?;
        let brush_properties = D2D1_BITMAP_BRUSH_PROPERTIES {
            extendModeX: D2D1_EXTEND_MODE_WRAP,
            extendModeY: D2D1_EXTEND_MODE_WRAP,
            interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
        };
        Ok(d2d.CreateBitmapBrush(&bitmap, Some(&brush_properties), None)?)
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

/// Snap the first line's baseline to a whole pixel without changing the text
/// format's line metrics.
fn snap_layout_y(layout: &IDWriteTextLayout, y: f32) -> f32 {
    let mut metrics = [DWRITE_LINE_METRICS::default(); 1];
    let mut count = 0u32;
    let _ = unsafe { layout.GetLineMetrics(Some(&mut metrics), &mut count) };
    if count > 0 {
        let baseline = metrics[0].baseline;
        (y.round() + baseline).round() - baseline
    } else {
        y.round()
    }
}

fn make_layout(
    dwrite: &IDWriteFactory,
    format: &IDWriteTextFormat,
    text: &str,
    width: f32,
    height: f32,
) -> Option<IDWriteTextLayout> {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        dwrite
            .CreateTextLayout(&utf16, format, width.max(1.0), height.max(1.0))
            .ok()
    }
}

fn color(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    let t = t.clamp(0.0, 1.0);
    Rect {
        left: a.left + (b.left - a.left) * t,
        top: a.top + (b.top - a.top) * t,
        right: a.right + (b.right - a.right) * t,
        bottom: a.bottom + (b.bottom - a.bottom) * t,
    }
}

/// Tick indices that can be visible for an unbounded ruler drag. Deriving the
/// range from the offset keeps the strip endless: it never runs out of marks
/// when the pointer is dragged far past the panel width.
fn tuner_tick_range(half_width: f32, offset: f32, gap: f32) -> (i32, i32) {
    if !(gap > 0.0) {
        return (0, 0);
    }
    let first = ((-half_width - offset) / gap).ceil() as i32 - 1;
    let last = ((half_width - offset) / gap).floor() as i32 + 1;
    (first, last)
}

/// Source rectangle that center-crops `data` to the aspect of `dest`, so a
/// wide banner keeps the thumbnail's proportions instead of stretching it.
fn cover_source_rect(data: &ImageData, dest: Rect) -> D2D_RECT_F {
    let image_w = data.width.max(1) as f32;
    let image_h = data.height.max(1) as f32;
    let dest_aspect = (dest.width() / dest.height().max(1.0)).max(0.01);
    let image_aspect = image_w / image_h;
    if image_aspect > dest_aspect {
        let width = image_h * dest_aspect;
        let left = (image_w - width) * 0.5;
        D2D_RECT_F {
            left,
            top: 0.0,
            right: left + width,
            bottom: image_h,
        }
    } else {
        let height = image_w / dest_aspect;
        let top = (image_h - height) * 0.5;
        D2D_RECT_F {
            left: 0.0,
            top,
            right: image_w,
            bottom: top + height,
        }
    }
}

fn to_d2d(rect: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.left,
        top: rect.top,
        right: rect.right.max(rect.left),
        bottom: rect.bottom.max(rect.top),
    }
}

fn compact_number(value: u64) -> String {
    if value >= 100_000_000 {
        format!("{:.1}亿", value as f64 / 100_000_000.0)
    } else if value >= 10_000 {
        format!("{:.1}万", value as f64 / 10_000.0)
    } else {
        value.to_string()
    }
}

/// Human bitrate for the performance panel; unknown direct streams read as a
/// dash rather than a fake zero.
fn format_bitrate(bits_per_second: u64) -> String {
    if bits_per_second == 0 {
        "—".to_string()
    } else if bits_per_second >= 1_000_000 {
        format!("{:.2} Mbps", bits_per_second as f64 / 1_000_000.0)
    } else if bits_per_second >= 1_000 {
        format!("{:.0} kbps", bits_per_second as f64 / 1_000.0)
    } else {
        format!("{bits_per_second} bps")
    }
}

/// Compact byte amount for data totals and the loading-grid caption.
fn format_bytes(bytes: f64) -> String {
    if !bytes.is_finite() || bytes <= 0.0 {
        return "—".to_string();
    }
    if bytes >= 1_073_741_824.0 {
        format!("{:.2} GB", bytes / 1_073_741_824.0)
    } else if bytes >= 1_048_576.0 {
        format!("{:.1} MB", bytes / 1_048_576.0)
    } else if bytes >= 1_024.0 {
        format!("{:.0} KB", bytes / 1_024.0)
    } else {
        format!("{bytes:.0} B")
    }
}

fn format_date(unix: i64) -> String {
    if unix <= 0 {
        return String::new();
    }
    let days = unix / 86_400;
    let (year, month, day) = o11a1::civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Relative comment time for the row's second header line.
fn format_comment_time(unix: i64) -> String {
    if unix <= 0 {
        return String::new();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(unix);
    let delta = (now - unix).max(0);
    if delta < 60 {
        "刚刚".to_string()
    } else if delta < 3_600 {
        format!("{} 分钟前", delta / 60)
    } else if delta < 86_400 {
        format!("{} 小时前", delta / 3_600)
    } else if delta < 172_800 {
        "昨天".to_string()
    } else if delta < 604_800 {
        format!("{} 天前", delta / 86_400)
    } else {
        format_date(unix)
    }
}

/// Fallback wrap for a machine where the GDI mask could not be created. It
/// keeps CJK at roughly one em and Latin at roughly half an em.
fn wrap_text_estimate(text: &str, width: f32, size: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0.0f32;
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut line));
            used = 0.0;
            continue;
        }
        let advance = if ch.is_ascii_whitespace() {
            size * 0.28
        } else if ch.is_ascii() {
            size * 0.56
        } else {
            size
        };
        if used + advance > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0.0;
        }
        line.push(ch);
        used += advance;
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

// Keep the unused width field warning-free while making the size explicit in
// the struct (useful in a debugger).
#[allow(dead_code)]
fn _size(overlay: &Overlay) -> (u32, u32) {
    (overlay.surface_width, overlay.surface_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_tab_indices_follow_the_rust_strip() {
        assert_eq!(SidebarTab::from_index(0), SidebarTab::Info);
        assert_eq!(SidebarTab::from_index(1), SidebarTab::Episodes);
        assert_eq!(SidebarTab::from_index(2), SidebarTab::Comments);
        assert_eq!(SidebarTab::from_index(3), SidebarTab::Performance);
        assert_eq!(SidebarTab::from_index(4), SidebarTab::Settings);
        assert_eq!(SidebarTab::Settings.label(), "设置");
    }

    #[test]
    fn precision_clear_reserves_a_visible_tail() {
        for width in [640.0, 800.0, 1200.0, 1600.0] {
            let layout = Layout::new(width, 700.0, 1.0, false, false);
            assert!(
                layout.precision_clear.width() >= 72.0,
                "clear chip too narrow at {width}: {:?}",
                layout.precision_clear
            );
            assert!(
                layout.precision_clear.left >= layout.precision_b.right,
                "clear chip overlaps B at {width}"
            );
            assert!(
                layout.precision_clear.right <= layout.precision.right,
                "clear chip leaves the panel at {width}"
            );
        }
    }

    #[test]
    fn loop_menu_has_four_rows_above_the_toolbar() {
        let layout = Layout::new(1000.0, 600.0, 1.0, true, false);
        assert_eq!(layout.loop_rows.len(), 4);
        assert!(layout.loop_menu.bottom <= layout.panel.top);
        for (index, row) in layout.loop_rows.iter().enumerate() {
            assert!(row.left >= layout.loop_menu.left);
            assert!(row.right <= layout.loop_menu.right);
            if index > 0 {
                assert!(layout.loop_rows[index - 1].bottom <= row.top);
            }
        }
    }

    #[test]
    fn endless_ruler_keeps_ticks_at_large_offsets() {
        for offset in [-5_000.0, -600.0, 0.0, 600.0, 5_000.0] {
            let (first, last) = tuner_tick_range(250.0, offset, 12.0);
            let left = first as f32 * 12.0 + offset;
            let right = last as f32 * 12.0 + offset;
            assert!(first < last, "empty tick range at {offset}");
            assert!(
                left <= -250.0 && right >= 250.0,
                "range does not cover the ruler at {offset}: {first}..{last}"
            );
        }
    }

    #[test]
    fn theme_colors_are_bound_before_set_color() {
        // Release builds turned the danger brushes into garbage when
        // `SetColor` received a `&color(...)` temporary, which made the
        // precision panel's 清空 chip disappear entirely. Keep the literals
        // split so this test's own source does not match the pattern.
        let source = include_str!("overlay.rs");
        let forbidden = format!("SetColor({}", "&color(");
        assert!(
            !source.contains(&forbidden),
            "bind theme colors to named locals before calling SetColor"
        );
    }

    #[test]
    fn context_menu_clamps_inside_the_window() {
        let top_left = dm_menu_rect(0.0, 0.0, 1280.0, 720.0, 1.0);
        assert!(top_left.left >= 8.0);
        assert!(top_left.top >= 8.0);
        assert!(top_left.right <= 1280.0 - 8.0 + 0.01);
        assert!(top_left.bottom <= 720.0 - 8.0 + 0.01);

        let bottom_right = dm_menu_rect(1279.0, 719.0, 1280.0, 720.0, 1.0);
        assert!(bottom_right.left >= 8.0);
        assert!(bottom_right.top >= 8.0);
        assert!(bottom_right.right <= 1272.0 + 0.01);
        assert!(bottom_right.bottom <= 712.0 + 0.01);
        // It stays adjacent to the pointer even after clamping.
        assert!(bottom_right.left < 1279.0);
        assert!(bottom_right.top < 719.0);
    }

    #[test]
    fn context_menu_rows_hit_in_order() {
        let menu = dm_menu_rect(200.0, 200.0, 1280.0, 720.0, 1.0);
        for index in 0..DM_MENU_LABELS.len() {
            let row = dm_menu_row(menu, index, 1.0);
            assert!(menu.contains((row.left + row.right) * 0.5, (row.top + row.bottom) * 0.5));
            if index + 1 < DM_MENU_LABELS.len() {
                let next = dm_menu_row(menu, index + 1, 1.0);
                assert!(row.bottom < next.top + 0.01);
            }
        }
    }

    #[test]
    fn report_menu_flips_left_near_the_right_edge() {
        let menu = dm_menu_rect(1270.0, 200.0, 1280.0, 720.0, 1.0);
        let report = dm_report_rect(menu, 1280.0, 720.0, 1.0);
        assert!(report.right <= 1272.0 + 0.01);
        // It cannot overlap the parent menu after the flip.
        assert!(report.right <= menu.left + 0.01);
        for index in 0..DM_REPORT_LABELS.len() {
            let row = dm_report_row(report, index, 1.0);
            assert!(report.contains((row.left + row.right) * 0.5, (row.top + row.bottom) * 0.5));
        }
    }

    #[test]
    fn fit_fills_one_axis_without_a_frame() {
        let layout = Layout::new(1000.0, 800.0, 1.0, false, false);
        assert_eq!(layout.stage.left, 0.0);
        assert_eq!(layout.stage.top, 0.0);
        assert_eq!(layout.stage.right, 1000.0);
        assert_eq!(layout.stage.bottom, 800.0);

        let (x, y, width, height) = layout.stage_video_bounds(1920, 1080, AspectMode::Fit);
        // 16:9 in a 5:4 window touches left/right and leaves only top/bottom
        // bars; there is no uniform frame on the other two sides.
        assert!((x - 0.0).abs() < 0.01);
        assert!((width - 1000.0).abs() < 0.01);
        assert!((y - 118.75).abs() < 0.01);
        assert!((height - 562.5).abs() < 0.01);
        assert!(y > 0.0);
        assert!(y + height < 800.0);
    }

    #[test]
    fn layout_has_five_tabs_and_the_rust_toolbar_grid() {
        let layout = Layout::new(1000.0, 600.0, 1.0, true, false);
        assert_eq!(layout.sidebar_tabs.len(), 5);
        // Windowed strip: full height and 360 logical px, Rust f214.
        assert_eq!(layout.sidebar.right - layout.sidebar.left, 360.0);
        assert_eq!(layout.sidebar.top, 0.0);
        assert_eq!(layout.sidebar.bottom, 600.0);
        // Pill: 10 | 22 seek | 8 | 36 | 10 = 86 tall, 482 wide at 100%.
        assert!((layout.panel.height() - 86.0).abs() < 0.01);
        assert!((layout.panel.width() - 482.0).abs() < 0.01);
        assert!((layout.loop_btn.width() - 36.0).abs() < 0.01);
        assert_eq!(layout.toolbar_buttons.len(), 9);
        // Cell order: precision, speed | mute, danmaku, info, comments, series
        // | performance, settings. Play and the clock column are gone.
        for pair in layout.toolbar_buttons.windows(2) {
            assert!(pair[0].right <= pair[1].left + 0.01);
        }
        assert!(layout.loop_btn.right <= layout.toolbar_buttons[TB_PRECISION].left);
        assert!(layout.divider1.right <= layout.toolbar_buttons[TB_MUTE].left);
        assert!(layout.divider2.right <= layout.toolbar_buttons[TB_PERF].left);
        assert!((layout.precision.width() - 626.0).abs() < 0.01);
        assert!((layout.speed_panel.height() - layout.panel.height()).abs() < 0.01);
        assert!((layout.speed_panel.width() - layout.panel.width()).abs() < 0.01);
        assert!((layout.speed_wheel.left - layout.speed_panel.left).abs() < 0.01);
        assert!((layout.speed_wheel.right - layout.speed_panel.right).abs() < 0.01);
        assert!((layout.speed_wheel.top - layout.speed_panel.top).abs() < 0.01);
        assert!((layout.speed_wheel.bottom - layout.speed_panel.bottom).abs() < 0.01);
        assert!(
            ((layout.precision.left + layout.precision.right)
                - (layout.panel.left + layout.panel.right))
                .abs()
                < 3.0
        );
        assert!(layout.track.bottom < layout.loop_btn.top);
        assert!(layout.undo_toast.bottom <= layout.panel.top);
        assert!(layout.undo_button.right <= layout.undo_toast.right);
        assert!(layout.undo_button.left >= layout.undo_toast.left);
        // The info tab reads top-down: cover, title, uploader, metrics, id,
        // actions, description. The id row and its copy target must stay
        // inside the side content, and the description owns the rest.
        assert!(layout.info_cover.bottom > layout.info_cover.top);
        assert!(layout.info_title.top >= layout.info_cover.bottom);
        assert!(layout.info_owner.top >= layout.info_title.bottom);
        assert!(layout.info_stats.top >= layout.info_owner.bottom);
        assert!(layout.info_id.top >= layout.info_stats.bottom);
        assert!(layout.info_id_copy.left > layout.info_id.left + 40.0);
        assert!(layout.info_id_copy.right <= layout.info_id.right);
        assert!(layout.info_actions[0].top > layout.info_id.bottom);
        assert!(layout.info_actions[2].right <= layout.sidebar_content.right + 0.01);
        assert!(layout.info_description.top > layout.info_actions[0].bottom);
    }
}
