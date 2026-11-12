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

use anyhow::Result;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Graphics::Gdi::{
    AddFontResourceExW, FONT_RESOURCE_CHARACTERISTICS, FR_NOT_ENUM, FR_PRIVATE,
};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_END_CLOSED, D2D1_PIXEL_FORMAT, D2D_SIZE_U, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_OPTIONS,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_BRUSH_PROPERTIES, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_ROUNDED_RECT,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, ID2D1Bitmap1, ID2D1Brush, ID2D1Device,
    ID2D1DeviceContext, ID2D1Factory1, ID2D1Geometry, ID2D1GeometrySink, ID2D1Image,
    ID2D1PathGeometry1, ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
    DWRITE_LINE_METRICS, DWRITE_PIXEL_GEOMETRY_RGB, DWRITE_RENDERING_MODE_GDI_CLASSIC,
    DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP, IDWriteFactory, IDWriteTextFormat,
    IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
};
use windows::Win32::Graphics::Dxgi::{IDXGISurface, IDXGISwapChain1};
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
    /// 弹幕历史: date list + historical comments in the live layer.
    History = 3,
    Performance = 4,
    Settings = 5,
}

impl SidebarTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "信息",
            Self::Episodes => "选集",
            Self::Comments => "评论",
            Self::History => "历史",
            Self::Performance => "性能",
            Self::Settings => "设置",
        }
    }

    pub fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Episodes,
            2 => Self::Comments,
            3 => Self::History,
            4 => Self::Performance,
            5 => Self::Settings,
            _ => Self::Info,
        }
    }
}

/// The 设置 tab's rows, in draw/hit-test order.
pub const SETTINGS_ROWS: usize = 13;

/// Which 设置 submenu is open over the tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsMenu {
    Quality,
    Audio,
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

#[derive(Clone, Default)]
pub struct CommentUi {
    pub user: String,
    pub message: String,
    pub likes: u64,
    pub avatar_url: String,
    pub avatar: Option<ImageData>,
}

#[derive(Clone, Default)]
pub struct InfoUi {
    pub title: String,
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
    /// Custom title strip; height 0 in fullscreen.
    pub top_bar: Rect,
    pub top_min: Rect,
    pub top_max: Rect,
    pub top_close: Rect,
    /// Right strip (300 logical px) when open.
    pub sidebar: Rect,
    pub sidebar_tabs: [Rect; 6],
    pub sidebar_close: Rect,
    pub sidebar_content: Rect,
    /// 历史 tab: 实时弹幕 row and date rows start below it.
    pub history_live: Rect,
    /// Compact centered toolbar. `panel` is kept as the toolbar alias.
    pub panel: Rect,
    /// Thin progress line above the toolbar.
    pub track: Rect,
    pub play: Rect,
    pub toolbar_buttons: [Rect; 11],
    /// 弹幕 settings popover.
    pub popup: Rect,
    pub popup_close: Rect,
    pub popup_opacity: Rect,
    pub popup_font: [Rect; 3],
    pub popup_speed: Rect,
    pub popup_area: [Rect; 4],
    pub popup_blocks: [Rect; 3],
    pub popup_density: Rect,
    pub popup_send: Rect,
    pub info_actions: [Rect; 3],
    pub comments_sort: [Rect; 2],
    pub comments_more: Rect,
    /// Full content overlay while a 设置 submenu is open.
    pub settings_menu: Rect,
    /// Retry button inside the centered error card.
    pub error_retry: Rect,
    pub stats: Rect,
    /// 3 s undo toast (屏蔽该用户) and its 撤销 button.
    pub undo_toast: Rect,
    pub undo_button: Rect,
    pub hints_y: f32,
}

pub const TB_VOLUME: usize = 0;
pub const TB_DANMAKU: usize = 1;
pub const TB_DANMAKU_SETTINGS: usize = 2;
pub const TB_SPEED: usize = 3;
pub const TB_ASPECT: usize = 4;
pub const TB_SHOT: usize = 5;
pub const TB_MINI: usize = 6;
pub const TB_FULLSCREEN: usize = 7;
pub const TB_PLAYLIST: usize = 8;
pub const TB_SIDEBAR: usize = 9;
pub const TB_SETTINGS: usize = 10;

impl Layout {
    pub fn new(width: f32, height: f32, scale: f32, sidebar_open: bool, fullscreen: bool) -> Self {
        let s = scale.max(0.75);
        let top_h = if fullscreen { 0.0 } else { 44.0 * s };
        let side_w = if sidebar_open { 300.0 * s } else { 0.0 };
        // The picture stays full-window. 非全屏 the strip is docked on the
        // right and the chrome (toolbar, popups, window buttons) centres in the
        // uncovered area; 全屏 it expands inward over the picture but still
        // keeps the chrome out from under it.
        let video_w = width;
        let ui_w = if sidebar_open {
            (width - side_w).max(320.0 * s)
        } else {
            width
        };
        let top_bar = Rect {
            left: 0.0,
            top: 0.0,
            right: video_w,
            bottom: top_h,
        };
        let toolbar_w = (640.0 * s).min((ui_w - 28.0 * s).max(420.0 * s));
        let toolbar_h = 54.0 * s;
        let toolbar_bottom = height - 16.0 * s;
        let toolbar = Rect {
            left: (ui_w - toolbar_w) * 0.5,
            top: toolbar_bottom - toolbar_h,
            right: (ui_w + toolbar_w) * 0.5,
            bottom: toolbar_bottom,
        };
        let track = Rect {
            left: toolbar.left + 14.0 * s,
            top: toolbar.top - 11.0 * s,
            right: toolbar.right - 14.0 * s,
            bottom: toolbar.top - 8.0 * s,
        };
        let play = Rect {
            left: toolbar.left + 14.0 * s,
            top: toolbar.top + 11.0 * s,
            right: toolbar.left + 46.0 * s,
            bottom: toolbar.top + 43.0 * s,
        };
        let mut toolbar_buttons = [Rect::default(); 11];
        let button = 30.0 * s;
        let gap = 4.0 * s;
        let start = toolbar.left + 150.0 * s;
        for (index, rect) in toolbar_buttons.iter_mut().enumerate() {
            let left = start + index as f32 * (button + gap);
            *rect = Rect {
                left,
                top: toolbar.top + 12.0 * s,
                right: left + button,
                bottom: toolbar.top + 42.0 * s,
            };
        }
        let sidebar = Rect {
            left: width - side_w,
            top: 0.0,
            right: width,
            bottom: height,
        };
        let tab_left = sidebar.left + 8.0 * s;
        let tab_right = sidebar.right - 42.0 * s;
        let tab_w = (tab_right - tab_left).max(1.0) / 6.0;
        let mut sidebar_tabs = [Rect::default(); 6];
        for (index, rect) in sidebar_tabs.iter_mut().enumerate() {
            let left = tab_left + index as f32 * tab_w;
            *rect = Rect {
                left,
                top: sidebar.top + 8.0 * s,
                right: left + tab_w,
                bottom: sidebar.top + 42.0 * s,
            };
        }
        let sidebar_content = Rect {
            left: sidebar.left + 14.0 * s,
            top: sidebar.top + 50.0 * s,
            right: sidebar.right - 14.0 * s,
            bottom: sidebar.bottom - 12.0 * s,
        };
        let history_live = Rect {
            left: sidebar_content.left,
            top: sidebar_content.top + 30.0 * s,
            right: sidebar_content.right,
            bottom: sidebar_content.top + 62.0 * s,
        };
        let popup_w = (420.0 * s).min((ui_w - 32.0 * s).max(300.0 * s));
        let popup_h = 300.0 * s;
        let popup_bottom = (toolbar.top - 18.0 * s).max(popup_h + 8.0 * s);
        let popup = Rect {
            left: (ui_w - popup_w) * 0.5,
            top: popup_bottom - popup_h,
            right: (ui_w + popup_w) * 0.5,
            bottom: popup_bottom,
        };
        let row = |y: f32, h: f32| Rect {
            left: popup.left + 14.0 * s,
            top: popup.top + y * s,
            right: popup.right - 14.0 * s,
            bottom: popup.top + (y + h) * s,
        };
        let popup_close = Rect {
            left: popup.right - 36.0 * s,
            top: popup.top + 8.0 * s,
            right: popup.right - 10.0 * s,
            bottom: popup.top + 34.0 * s,
        };
        let mut popup_font = [Rect::default(); 3];
        for (index, rect) in popup_font.iter_mut().enumerate() {
            let left = popup.left + 90.0 * s + index as f32 * 78.0 * s;
            *rect = Rect {
                left,
                top: popup.top + 78.0 * s,
                right: left + 70.0 * s,
                bottom: popup.top + 102.0 * s,
            };
        }
        let mut popup_area = [Rect::default(); 4];
        for (index, rect) in popup_area.iter_mut().enumerate() {
            let left = popup.left + 90.0 * s + index as f32 * 68.0 * s;
            *rect = Rect {
                left,
                top: popup.top + 160.0 * s,
                right: left + 60.0 * s,
                bottom: popup.top + 184.0 * s,
            };
        }
        let mut popup_blocks = [Rect::default(); 3];
        for (index, rect) in popup_blocks.iter_mut().enumerate() {
            let left = popup.left + 90.0 * s + index as f32 * 78.0 * s;
            *rect = Rect {
                left,
                top: popup.top + 198.0 * s,
                right: left + 70.0 * s,
                bottom: popup.top + 222.0 * s,
            };
        }
        let mut info_actions = [Rect::default(); 3];
        let action_top = sidebar_content.top + 150.0 * s;
        let action_w = (sidebar_content.width() - 12.0 * s) / 3.0;
        for (index, rect) in info_actions.iter_mut().enumerate() {
            let left = sidebar_content.left + index as f32 * (action_w + 6.0 * s);
            *rect = Rect {
                left,
                top: action_top,
                right: left + action_w,
                bottom: action_top + 32.0 * s,
            };
        }
        let sort_top = sidebar_content.top + 40.0 * s;
        let comments_sort = [
            Rect {
                left: sidebar_content.left,
                top: sort_top,
                right: sidebar_content.left + 58.0 * s,
                bottom: sort_top + 26.0 * s,
            },
            Rect {
                left: sidebar_content.left + 64.0 * s,
                top: sort_top,
                right: sidebar_content.left + 122.0 * s,
                bottom: sort_top + 26.0 * s,
            },
        ];
        let comments_more = Rect {
            left: sidebar_content.left,
            top: sidebar_content.bottom - 32.0 * s,
            right: sidebar_content.right,
            bottom: sidebar_content.bottom,
        };
        let settings_menu = sidebar_content;
        let error_h = 156.0 * s;
        let error_top = (height * 0.5 - error_h * 0.5).max(top_h + 8.0 * s);
        let error_retry = Rect {
            left: (width - 96.0 * s) * 0.5,
            top: error_top + error_h - 48.0 * s,
            right: (width + 96.0 * s) * 0.5,
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
        Self {
            scale: s,
            width,
            height,
            top_bar,
            top_min: Rect {
                left: ui_w - 138.0 * s,
                top: 0.0,
                right: ui_w - 92.0 * s,
                bottom: top_h,
            },
            top_max: Rect {
                left: ui_w - 92.0 * s,
                top: 0.0,
                right: ui_w - 46.0 * s,
                bottom: top_h,
            },
            top_close: Rect {
                left: ui_w - 46.0 * s,
                top: 0.0,
                right: ui_w,
                bottom: top_h,
            },
            sidebar,
            sidebar_tabs,
            sidebar_close: Rect {
                left: sidebar.right - 38.0 * s,
                top: sidebar.top + 10.0 * s,
                right: sidebar.right - 10.0 * s,
                bottom: sidebar.top + 38.0 * s,
            },
            sidebar_content,
            history_live,
            panel: toolbar,
            track,
            play,
            toolbar_buttons,
            popup,
            popup_close,
            popup_opacity: row(40.0, 8.0),
            popup_font,
            popup_speed: row(120.0, 8.0),
            popup_area,
            popup_blocks,
            popup_density: Rect {
                left: popup.left + 90.0 * s,
                top: popup.top + 236.0 * s,
                right: popup.left + 200.0 * s,
                bottom: popup.top + 260.0 * s,
            },
            popup_send: Rect {
                left: popup.left + 90.0 * s,
                top: popup.top + 262.0 * s,
                right: popup.right - 14.0 * s,
                bottom: popup.top + 290.0 * s,
            },
            info_actions,
            comments_sort,
            comments_more,
            settings_menu,
            error_retry,
            stats: sidebar_content,
            undo_toast,
            undo_button,
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

    pub fn progress_fraction(&self, x: f32) -> f32 {
        ((x - self.track.left) / self.track.width().max(1.0)).clamp(0.0, 1.0)
    }

    pub fn comment_row(&self, index: usize, scroll: f32) -> Rect {
        let h = 72.0 * self.scale;
        let top = self.sidebar_content.top + 76.0 * self.scale + index as f32 * h - scroll;
        Rect {
            left: self.sidebar_content.left,
            top,
            right: self.sidebar_content.right,
            bottom: top + h - 4.0 * self.scale,
        }
    }

    pub fn episode_row(&self, index: usize, scroll: f32) -> Rect {
        let h = 44.0 * self.scale;
        let top = self.sidebar_content.top + 8.0 * self.scale + index as f32 * h - scroll;
        Rect {
            left: self.sidebar_content.left,
            top,
            right: self.sidebar_content.right,
            bottom: top + h - 4.0 * self.scale,
        }
    }

    pub fn settings_row(&self, index: usize, scroll: f32) -> Rect {
        let h = 38.0 * self.scale;
        let top = self.sidebar_content.top + index as f32 * h - scroll;
        Rect {
            left: self.sidebar_content.left,
            top,
            right: self.sidebar_content.right,
            bottom: top + h - 2.0 * self.scale,
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

    /// Row `index` inside the open 设置 submenu (title occupies the first row).
    pub fn settings_menu_row(&self, index: usize, scroll: f32) -> Rect {
        let h = 34.0 * self.scale;
        let top = self.settings_menu.top + 34.0 * self.scale
            + index as f32 * h
            - scroll;
        Rect {
            left: self.settings_menu.left,
            top,
            right: self.settings_menu.right,
            bottom: top + h - 2.0 * self.scale,
        }
    }
}

/// The state the overlay draws. One value per redraw; no video pixels.
pub struct UiModel {
    pub title: String,
    pub playing: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub volume: u8,
    pub muted: bool,
    pub interp: InterpMode,
    pub enhance: EnhanceMode,
    pub speed: f32,
    pub fps: f32,
    pub source_fps: f64,
    pub generated: u64,
    pub blend_us: u32,
    pub stats: bool,
    pub status: String,
    pub source_size: (u32, u32),
    pub qn_label: String,
    pub codec: String,
    pub drag_preview: Option<u64>,
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
    pub comments_hot: bool,
    pub comments_loading: bool,
    pub comments_end: bool,
    pub liked: bool,
    pub coined: bool,
    pub favorited: bool,
    pub toast: Option<ToastUi>,
    pub quality_label: String,
    pub audio_label: String,
    /// 画质 menu rows (playable streams + locked 大会员 tiers).
    pub quality_rows: Vec<QualityRowUi>,
    /// 音轨 menu rows.
    pub audio_rows: Vec<AudioRowUi>,
    pub settings_menu: Option<SettingsMenu>,
    pub sleep_label: String,
    pub keys_label: String,
    pub hover_preview: Option<u64>,
    pub storyboard_cell: Option<StoryboardCellUi>,
    pub light_sidebar: bool,
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
    pub audio_ms: u64,
    pub ui_ms: f32,
    pub renderer: String,
}

// ------------------------------------------------------------------ renderer

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
    hint_format: IDWriteTextFormat,
    tab_format: IDWriteTextFormat,
    wrap_body: IDWriteTextFormat,
    wrap_small: IDWriteTextFormat,
    text: ID2D1SolidColorBrush,
    text_dim: ID2D1SolidColorBrush,
    text_faint: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
    panel: ID2D1SolidColorBrush,
    panel_border: ID2D1SolidColorBrush,
    chip: ID2D1SolidColorBrush,
    hint_band: ID2D1SolidColorBrush,
    stats_panel: ID2D1SolidColorBrush,
    track: ID2D1SolidColorBrush,
    black: ID2D1SolidColorBrush,
    chrome: ID2D1SolidColorBrush,
    chrome_soft: ID2D1SolidColorBrush,
    sidebar_line: ID2D1SolidColorBrush,
    hover: ID2D1SolidColorBrush,
    accent_blue: ID2D1SolidColorBrush,
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
    /// Top strip stays dark in both themes, like the old player title bar.
    top_bg: ID2D1SolidColorBrush,
    top_line: ID2D1SolidColorBrush,
    top_text: ID2D1SolidColorBrush,
    top_text_dim: ID2D1SolidColorBrush,
    top_hover: ID2D1SolidColorBrush,
    play_geometry: ID2D1PathGeometry1,
    bitmaps: HashMap<u64, ID2D1Bitmap1>,
    /// Hinted greyscale text masks; `None` when GDI could not initialise.
    gdi: std::cell::RefCell<Option<gdi_text::GdiText>>,
    styles: HashMap<usize, gdi_text::Style>,
    text_ok: bool,
    width: u32,
    height: u32,
    layout_sidebar: bool,
    layout_fullscreen: bool,
    pub layout: Layout,
    pub frames: u64,
}

impl Overlay {
    pub fn new(gpu: &Gpu, width: u32, height: u32, scale: f32) -> Result<Self> {
        let (swap, _) = gpu.create_swapchain(width, height, DXGI_ALPHA_MODE_PREMULTIPLIED, false)?;
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
            let hint_format = text_format(&dwrite, 11.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            hint_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            let tab_format = text_format(&dwrite, 12.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            tab_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            let wrap_body = text_format(&dwrite, 12.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            wrap_body.SetWordWrapping(windows::Win32::Graphics::DirectWrite::DWRITE_WORD_WRAPPING_WRAP)?;
            let wrap_small = text_format(&dwrite, 11.0 * s, DWRITE_FONT_WEIGHT_NORMAL)?;
            wrap_small.SetWordWrapping(windows::Win32::Graphics::DirectWrite::DWRITE_WORD_WRAPPING_WRAP)?;

            let text = solid(&render_target, color(242, 245, 250, 1.0))?;
            let text_dim = solid(&render_target, color(205, 213, 226, 0.78))?;
            let text_faint = solid(&render_target, color(214, 221, 232, 0.55))?;
            let accent = solid(&render_target, color(251, 114, 153, 1.0))?;
            let panel = solid(&render_target, color(11, 14, 19, 0.68))?;
            let panel_border = solid(&render_target, color(255, 255, 255, 0.10))?;
            let chip = solid(&render_target, color(255, 255, 255, 0.08))?;
            let hint_band = solid(&render_target, color(6, 8, 12, 0.34))?;
            let stats_panel = solid(&render_target, color(8, 10, 15, 0.92))?;
            let track = solid(&render_target, color(255, 255, 255, 0.34))?;
            let black = solid(&render_target, color(3, 4, 6, 1.0))?;
            let chrome = solid(&render_target, color(15, 17, 22, 0.88))?;
            let chrome_soft = solid(&render_target, color(20, 22, 28, 0.72))?;
            let sidebar_line = solid(&render_target, color(255, 255, 255, 0.08))?;
            let hover = solid(&render_target, color(255, 255, 255, 0.12))?;
            let accent_blue = solid(&render_target, color(64, 156, 255, 1.0))?;
            let toast_bg = solid(&render_target, color(28, 30, 38, 0.96))?;
            let sidebar_text = solid(&render_target, color(242, 245, 250, 1.0))?;
            let sidebar_text_dim = solid(&render_target, color(205, 213, 226, 0.78))?;
            let sidebar_text_faint = solid(&render_target, color(214, 221, 232, 0.55))?;
            let sidebar_panel_bg = solid(&render_target, color(16, 17, 22, 0.95))?;
            let sidebar_panel_line = solid(&render_target, color(255, 255, 255, 0.08))?;
            let sidebar_chip = solid(&render_target, color(255, 255, 255, 0.08))?;
            let sidebar_hover = solid(&render_target, color(255, 255, 255, 0.12))?;
            let sidebar_avatar_bg = solid(&render_target, color(52, 58, 70, 1.0))?;
            let top_bg = solid(&render_target, color(15, 17, 22, 0.88))?;
            let top_line = solid(&render_target, color(255, 255, 255, 0.08))?;
            let top_text = solid(&render_target, color(242, 245, 250, 1.0))?;
            let top_text_dim = solid(&render_target, color(205, 213, 226, 0.78))?;
            let top_hover = solid(&render_target, color(255, 255, 255, 0.14))?;

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
                (&hint_format, 11.0 * s, 400),
                (&tab_format, 12.0 * s, 400),
                (&wrap_body, 12.0 * s, 400),
                (&wrap_small, 11.0 * s, 400),
            ] {
                styles.insert(
                    windows::core::Interface::as_raw(format) as usize,
                    text_style(&dwrite, format, size, weight),
                );
            }
            let text_ok = player_font_available();
            let gdi = std::cell::RefCell::new(if !text_ok
                || std::env::var("O11A1_PLAYER_GDI")
                    .map(|value| value == "0")
                    .unwrap_or(false)
            {
                None
            } else {
                gdi_text::GdiText::new(player_font_family())
            });

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
                hint_format,
                tab_format,
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
                track,
                black,
                chrome,
                chrome_soft,
                sidebar_line,
                hover,
                accent_blue,
                toast_bg,
                sidebar_text,
                sidebar_text_dim,
                sidebar_text_faint,
                sidebar_panel_bg,
                sidebar_panel_line,
                sidebar_chip,
                sidebar_hover,
                sidebar_avatar_bg,
                top_bg,
                top_line,
                top_text,
                top_text_dim,
                top_hover,
                play_geometry,
                bitmaps: HashMap::new(),
                gdi,
                styles,
                text_ok,
                width,
                height,
                layout_sidebar: false,
                layout_fullscreen: false,
                layout: Layout::new(width as f32, height as f32, s, false, false),
                frames: 0,
            })
        }
    }

    pub fn resize(&mut self, width: u32, height: u32, scale: f32) -> Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            self.layout = Layout::new(
                width as f32,
                height as f32,
                scale,
                self.layout_sidebar,
                self.layout_fullscreen,
            );
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
                color(31, 36, 44, 1.0),
                color(80, 88, 102, 0.94),
                color(120, 128, 142, 0.85),
                color(246, 247, 250, 0.98),
                color(0, 0, 0, 0.10),
                color(0, 0, 0, 0.06),
                color(0, 0, 0, 0.10),
                color(205, 212, 222, 1.0),
            ]
        } else {
            [
                color(242, 245, 250, 1.0),
                color(205, 213, 226, 0.78),
                color(214, 221, 232, 0.55),
                color(16, 17, 22, 0.95),
                color(255, 255, 255, 0.08),
                color(255, 255, 255, 0.08),
                color(255, 255, 255, 0.12),
                color(52, 58, 70, 1.0),
            ]
        };
        let chrome = if light {
            [
                color(255, 255, 255, 0.92),  // chrome
                color(255, 255, 255, 0.84),  // chrome_soft
                color(255, 255, 255, 0.95),  // panel
                color(0, 0, 0, 0.08),        // panel_border
                color(0, 0, 0, 0.055),       // chip
                color(0, 0, 0, 0.18),        // track
                color(26, 26, 26, 1.0),      // text
                color(90, 90, 98, 0.94),     // text_dim
                color(107, 107, 114, 0.85),  // text_faint
                color(255, 255, 255, 0.96),  // stats_panel
                color(255, 255, 255, 0.97),  // toast_bg
                color(0, 0, 0, 0.06),        // hover
            ]
        } else {
            [
                color(15, 17, 22, 0.88),
                color(20, 22, 28, 0.72),
                color(11, 14, 19, 0.74),
                color(255, 255, 255, 0.10),
                color(255, 255, 255, 0.08),
                color(255, 255, 255, 0.34),
                color(242, 245, 250, 1.0),
                color(205, 213, 226, 0.78),
                color(214, 221, 232, 0.55),
                color(8, 10, 15, 0.92),
                color(28, 30, 38, 0.96),
                color(255, 255, 255, 0.12),
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
            // Old progress/play accent is blue in both themes.
            let _ = self.accent.SetColor(&color(47, 155, 255, 1.0));
        }
    }

    #[allow(dead_code)]
    fn draw_legacy(&mut self, model: &UiModel) -> Result<()> {
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
            let bitmap: ID2D1Bitmap1 = self.d2d.CreateBitmapFromDxgiSurface(&surface, Some(&properties))?;
            let image: ID2D1Image = bitmap.cast()?;
            self.d2d.SetTarget(&image);
            self.d2d.BeginDraw();
            self.d2d.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));

            // Opaque letterbox bars only in 适应. 填充 crops and 拉伸 fills, so
            // those modes must not paint bars over the picture. The video
            // visual sits exactly in the remaining rect, so the window never
            // shows the desktop through it.
            if model.aspect == AspectMode::Fit {
                let (vx, vy, video_width, video_height) = crate::gpu::video_bounds(
                    layout.width,
                    layout.height,
                    model.source_size.0,
                    model.source_size.1,
                    model.aspect,
                );
                let video_right = vx + video_width;
                let video_bottom = vy + video_height;
                if vy > 0.5 {
                    self.fill(
                        Rect {
                            left: 0.0,
                            top: 0.0,
                            right: layout.width,
                            bottom: vy,
                        },
                        &self.black,
                    );
                    self.fill(
                        Rect {
                            left: 0.0,
                            top: video_bottom,
                            right: layout.width,
                            bottom: layout.height,
                        },
                        &self.black,
                    );
                }
                if vx > 0.5 {
                    self.fill(
                        Rect {
                            left: 0.0,
                            top: vy,
                            right: vx,
                            bottom: video_bottom,
                        },
                        &self.black,
                    );
                    self.fill(
                        Rect {
                            left: video_right,
                            top: vy,
                            right: layout.width,
                            bottom: video_bottom,
                        },
                        &self.black,
                    );
                }
            }

            if model.mini {
                // Mini mode: only the letterbox (above) and a transient
                // restore hint. No title, stats, progress, controls or hints.
                if model.mini_hint {
                    let hint = Rect {
                        left: (layout.width - 132.0 * s - 8.0 * s).max(8.0 * s),
                        top: 8.0 * s,
                        right: (layout.width - 8.0 * s).max(20.0 * s),
                        bottom: 8.0 * s + 26.0 * s,
                    };
                    let rounded = D2D1_ROUNDED_RECT {
                        rect: to_d2d(hint),
                        radiusX: 13.0 * s,
                        radiusY: 13.0 * s,
                    };
                    self.d2d.FillRoundedRectangle(&rounded, &self.stats_panel);
                    self.d2d
                        .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
                    self.text_centered(
                        "Ctrl+M 还原",
                        hint,
                        &self.small_format,
                        &self.text,
                    );
                }
                let _ = self.d2d.EndDraw(None, None);
                let _ = self.d2d.SetTarget(None::<&ID2D1Image>);
                drop(image);
                drop(bitmap);
                let _ = present_ui(&self.swap);
                self.frames = self.frames.saturating_add(1);
                return Ok(());
            }

            // Top glass bar (dark in both themes).
            self.fill(layout.top_bar, &self.top_bg);
            self.fill(
                Rect {
                    left: 0.0,
                    top: layout.top_bar.bottom - s,
                    right: layout.width,
                    bottom: layout.top_bar.bottom,
                },
                &self.top_line,
            );
            let title = ellipsize(&model.title, 54);
            self.text(
                &title,
                layout.top_bar.left + 20.0 * s,
                layout.top_bar.top + 16.0 * s,
                (layout.width * 0.62).max(160.0),
                &self.title_format,
                &self.top_text,
            );
            let codec = format!(
                "{} · {} · {:.0} fps",
                if model.qn_label.is_empty() {
                    "视频"
                } else {
                    &model.qn_label
                },
                if model.codec.is_empty() {
                    "自动"
                } else {
                    &model.codec
                },
                model.source_fps
            );
            self.text_right(
                &codec,
                layout.width - 20.0 * s,
                layout.top_bar.top + 18.0 * s,
                280.0 * s,
                &self.small_format,
                &self.top_text_dim,
            );

            // Keyboard hints just above the control panel. A faint band keeps
            // them legible over bright video without competing with the picture.
            if layout.width > 760.0 * s {
                self.fill(
                    Rect {
                        left: layout.panel.left,
                        top: layout.hints_y - 2.0 * s,
                        right: layout.panel.right,
                        bottom: layout.hints_y + 36.0 * s,
                    },
                    &self.hint_band,
                );
                self.text_centered(
                    "空格 播放/暂停  ·  ←/→ 快退/快进  ·  ↑/↓ 音量  ·  M 静音  ·  D 弹幕  ·  F 全屏  ·  I 插帧  ·  E 画质增强  ·  P 统计",
                    Rect {
                        left: layout.panel.left,
                        top: layout.hints_y,
                        right: layout.panel.right,
                        bottom: layout.hints_y + 17.0 * s,
                    },
                    &self.hint_format,
                    &self.text_faint,
                );
                self.text_centered(
                    "S 截图  ·  A 比例  ·  T 置顶  ·  -/= 亮度  ·  V 仅音频  ·  [ ] 速度  ·  Ctrl+M 迷你  ·  Esc 退出",
                    Rect {
                        left: layout.panel.left,
                        top: layout.hints_y + 17.0 * s,
                        right: layout.panel.right,
                        bottom: layout.hints_y + 34.0 * s,
                    },
                    &self.hint_format,
                    &self.text_faint,
                );
            }

            // Bottom glass panel.
            let panel = D2D1_ROUNDED_RECT {
                rect: to_d2d(layout.panel),
                radiusX: 14.0 * s,
                radiusY: 14.0 * s,
            };
            self.d2d.FillRoundedRectangle(&panel, &self.panel);
            self.d2d
                .DrawRoundedRectangle(&panel, &self.panel_border, 1.0 * s, None);

            // Progress bar.
            let position = model.drag_preview.unwrap_or(model.position_ms);
            let fraction = if model.duration_ms > 0 {
                (position as f32 / model.duration_ms as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let track = D2D1_ROUNDED_RECT {
                rect: to_d2d(layout.track),
                radiusX: 2.5 * s,
                radiusY: 2.5 * s,
            };
            self.d2d.FillRoundedRectangle(&track, &self.track);
            let filled_right = layout.track.left + layout.track.width() * fraction;
            let filled = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: layout.track.left,
                    top: layout.track.top,
                    right: filled_right.max(layout.track.left + 2.0 * s),
                    bottom: layout.track.bottom,
                },
                radiusX: 2.5 * s,
                radiusY: 2.5 * s,
            };
            self.d2d.FillRoundedRectangle(&filled, &self.accent);
            let thumb = Vector2::new(filled_right, layout.track.top + 2.0 * s);
            self.d2d.FillEllipse(
                &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                    point: thumb,
                    radiusX: 6.5 * s,
                    radiusY: 6.5 * s,
                },
                &self.text,
            );
            if model.drag_preview.is_some() {
                let bubble = Rect {
                    left: (thumb.X - 34.0 * s).max(4.0 * s),
                    top: layout.track.top - 30.0 * s,
                    right: thumb.X + 34.0 * s,
                    bottom: layout.track.top - 6.0 * s,
                };
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(bubble),
                    radiusX: 7.0 * s,
                    radiusY: 7.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.chip);
                self.text_centered(
                    &format_time(position),
                    bubble,
                    &self.small_format,
                    &self.text,
                );
            }

            // Play/pause glyph + time.
            let play_center = Vector2::new(
                (layout.play.left + layout.play.right) * 0.5,
                (layout.play.top + layout.play.bottom) * 0.5,
            );
            self.d2d.FillEllipse(
                &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                    point: play_center,
                    radiusX: layout.play.width() * 0.5,
                    radiusY: layout.play.height() * 0.5,
                },
                &self.chip,
            );
            if model.playing {
                let bar_w = 4.0 * s;
                let bar_h = 13.0 * s;
                let gap = 4.0 * s;
                for sign in [-1.0f32, 1.0] {
                    self.fill(
                        Rect {
                            left: play_center.X + sign * gap - bar_w * 0.5,
                            top: play_center.Y - bar_h * 0.5,
                            right: play_center.X + sign * gap + bar_w * 0.5,
                            bottom: play_center.Y + bar_h * 0.5,
                        },
                        &self.text,
                    );
                }
            } else {
                let scale = 10.5 * s;
                let origin_x = play_center.X - 4.0 * s;
                let origin_y = play_center.Y - 6.2 * s;
                let transform = Matrix3x2 {
                    M11: scale,
                    M12: 0.0,
                    M21: 0.0,
                    M22: scale,
                    M31: origin_x,
                    M32: origin_y,
                };
                self.render_target.SetTransform(&transform);
                let geometry: ID2D1Geometry = self.play_geometry.cast()?;
                let brush: ID2D1Brush = self.text.cast()?;
                self.render_target.FillGeometry(&geometry, &brush, None::<&ID2D1Brush>);
                self.render_target.SetTransform(&Matrix3x2::identity());
            }
            let time = format!(
                "{} / {}",
                format_time(position),
                format_time(model.duration_ms)
            );
            self.text(
                &time,
                layout.play.right + 14.0 * s,
                play_center.Y - 9.0 * s,
                230.0 * s,
                &self.body_format,
                &self.text,
            );
            if (model.speed - 1.0).abs() > 0.01 {
                self.text(
                    &format!("{:.2}×", model.speed),
                    layout.play.right + 160.0 * s,
                    play_center.Y - 8.0 * s,
                    80.0 * s,
                    &self.small_format,
                    &self.accent,
                );
            }

            // Right-side chips: 弹幕, 音量, 插帧, 画质增强.
            let chip_y = layout.panel.top + 62.0 * s;
            let chip_h = 30.0 * s;
            let enhance_label = format!("画质增强: {}", model.enhance.label());
            let interp_label = format!("插帧: {}", model.interp.label());
            let volume_label = if model.muted {
                "音量: 静音".to_string()
            } else {
                format!("音量: {}%", model.volume)
            };
            let danmaku_label = if model.danmaku_visible {
                "弹幕: 开".to_string()
            } else {
                "弹幕: 关".to_string()
            };
            let right = layout.panel.right - 18.0 * s;
            let enhance_rect = Rect {
                left: right - 126.0 * s,
                top: chip_y,
                right,
                bottom: chip_y + chip_h,
            };
            let interp_rect = Rect {
                left: enhance_rect.left - 112.0 * s,
                top: chip_y,
                right: enhance_rect.left - 8.0 * s,
                bottom: chip_y + chip_h,
            };
            let volume_rect = Rect {
                left: interp_rect.left - 122.0 * s,
                top: chip_y,
                right: interp_rect.left - 8.0 * s,
                bottom: chip_y + chip_h,
            };
            let danmaku_rect = Rect {
                left: volume_rect.left - 92.0 * s,
                top: chip_y,
                right: volume_rect.left - 8.0 * s,
                bottom: chip_y + chip_h,
            };
            let aspect_label = format!("比例: {}", model.aspect.label());
            let ontop_label = if model.ontop {
                "置顶: 开".to_string()
            } else {
                "置顶: 关".to_string()
            };
            let brightness_label = format!("亮度: {:.0}%", model.brightness * 100.0);
            let audio_label = if model.audio_only {
                "仅音频: 开".to_string()
            } else {
                "仅音频: 关".to_string()
            };
            let mut chips: Vec<(Rect, String, bool)> = vec![
                (danmaku_rect, danmaku_label, model.danmaku_visible),
                (volume_rect, volume_label, !model.muted),
                (interp_rect, interp_label, model.interp != InterpMode::Off),
                (enhance_rect, enhance_label, model.enhance != EnhanceMode::Off),
            ];
            if layout.width > 1220.0 * s {
                let aspect_rect = Rect {
                    left: danmaku_rect.left - 104.0 * s,
                    top: chip_y,
                    right: danmaku_rect.left - 8.0 * s,
                    bottom: chip_y + chip_h,
                };
                let ontop_rect = Rect {
                    left: aspect_rect.left - 92.0 * s,
                    top: chip_y,
                    right: aspect_rect.left - 8.0 * s,
                    bottom: chip_y + chip_h,
                };
                let brightness_rect = Rect {
                    left: ontop_rect.left - 118.0 * s,
                    top: chip_y,
                    right: ontop_rect.left - 8.0 * s,
                    bottom: chip_y + chip_h,
                };
                let audio_rect = Rect {
                    left: brightness_rect.left - 104.0 * s,
                    top: chip_y,
                    right: brightness_rect.left - 8.0 * s,
                    bottom: chip_y + chip_h,
                };
                chips.push((audio_rect, audio_label, model.audio_only));
                chips.push((
                    brightness_rect,
                    brightness_label,
                    (model.brightness - 1.0).abs() > 0.001,
                ));
                chips.push((ontop_rect, ontop_label, model.ontop));
                chips.push((aspect_rect, aspect_label, model.aspect != AspectMode::Fit));
            }
            for (rect, label, active) in &chips {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(*rect),
                    radiusX: chip_h * 0.5,
                    radiusY: chip_h * 0.5,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.chip);
                self.text_centered(
                    label,
                    *rect,
                    &self.small_format,
                    if *active { &self.text } else { &self.text_dim },
                );
            }
            // A thin volume fill under the label, so the value reads at a glance.
            if layout.width > 980.0 * s {
                let inner_left = volume_rect.left + 10.0 * s;
                let inner_width = (volume_rect.width() - 20.0 * s).max(1.0);
                let fraction = if model.muted {
                    0.0
                } else {
                    model.volume as f32 / 100.0
                };
                self.fill(
                    Rect {
                        left: inner_left,
                        top: volume_rect.bottom - 5.0 * s,
                        right: inner_left + inner_width * fraction,
                        bottom: volume_rect.bottom - 3.0 * s,
                    },
                    &self.accent,
                );
            }

            // Center status (loading/buffering). Drawn over the video but under
            // the bars.
            if !model.status.is_empty() {
                let status_w = 180.0 * s;
                let rect = Rect {
                    left: (layout.width - status_w) * 0.5,
                    top: layout.height * 0.5 - 20.0 * s,
                    right: (layout.width + status_w) * 0.5,
                    bottom: layout.height * 0.5 + 20.0 * s,
                };
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(rect),
                    radiusX: 20.0 * s,
                    radiusY: 20.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.panel);
                self.text_centered(&model.status, rect, &self.body_format, &self.text);
            }

            // Stats panel.
            if model.stats {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(layout.stats),
                    radiusX: 12.0 * s,
                    radiusY: 12.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.stats_panel);
                self.d2d
                    .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
                let lines = [
                    format!("fps        {:.1}  源 {:.1}", model.fps, model.source_fps),
                    format!("位置       {}", crate::format::format_time_ms(model.position_ms)),
                    format!(
                        "生成帧     {}  混合 {:.2} ms",
                        model.generated,
                        model.blend_us as f32 / 1000.0
                    ),
                    format!(
                        "分辨率     {}×{}",
                        model.source_size.0, model.source_size.1
                    ),
                    format!("插帧       {}", model.interp.label()),
                    format!("画质增强   {}", model.enhance.label()),
                    format!(
                        "弹幕       {}（屏上 {}）",
                        model.danmaku_total, model.danmaku_active
                    ),
                    format!("速度       {:.2}×", model.speed),
                    format!("比例       {}", model.aspect.label()),
                    format!("置顶       {}", if model.ontop { "开" } else { "关" }),
                    format!("亮度       {:.0}%", model.brightness * 100.0),
                    format!(
                        "仅音频     {}",
                        if model.audio_only { "开" } else { "关" }
                    ),
                ];
                for (index, line) in lines.iter().enumerate() {
                    self.text(
                        line,
                        layout.stats.left + 14.0 * s,
                        layout.stats.top + 12.0 * s + index as f32 * 21.0 * s,
                        layout.stats.width() - 28.0 * s,
                        &self.small_format,
                        &self.text_dim,
                    );
                }
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
        }
        let _ = present_ui(&self.swap);
        Ok(())
    }

    /// Draw only a storyboard thumbnail above the progress line. Used by the
    /// Electron-hosted popup: the native chrome stays hidden, but the video
    /// layer can still show a real hover preview.
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
            let target: ID2D1Image = bitmap.cast()?;
            self.d2d.SetTarget(&target);
            self.d2d.BeginDraw();
            self.d2d.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            let image_w = 176.0 * s;
            let image_h = 99.0 * s;
            let anchor_x = layout.track.left + layout.track.width() * fraction.clamp(0.0, 1.0);
            let left = (anchor_x - image_w * 0.5)
                .clamp(8.0 * s, (layout.width - image_w - 8.0 * s).max(8.0 * s));
            let top = (layout.track.top - image_h - 12.0 * s)
                .max(layout.top_bar.bottom + 4.0 * s);
            let frame = Rect {
                left,
                top,
                right: left + image_w,
                bottom: top + image_h,
            };
            if image.width > 0 && image.height > 0 {
                let _ = self.draw_image(image, frame);
                self.stroke_rect(frame, 1.5 * s, &self.panel_border);
            }
            let _ = self.d2d.EndDraw(None, None);
        }
        let _ = present_ui(&self.swap);
        Ok(())
    }

    pub fn draw(&mut self, model: &UiModel) -> Result<()> {
        if model.mini {
            // Mini keeps the proven compact layer: letterbox + restore hint.
            return self.draw_legacy(model);
        }
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

            // The loading screen must be fully opaque: the video swapchain has
            // not presented a frame yet, so a transparent UI layer would let
            // the desktop show through.
            if model.splash {
                self.fill(
                    Rect {
                        left: 0.0,
                        top: 0.0,
                        right: layout.width,
                        bottom: layout.height,
                    },
                    &self.black,
                );
            }

            // Fit letterbox. Fill/Stretch crop or fill underneath the chrome.
            if model.aspect == AspectMode::Fit {
                let (vx, vy, video_width, video_height) = crate::gpu::video_bounds(
                    layout.width,
                    layout.height,
                    model.source_size.0,
                    model.source_size.1,
                    model.aspect,
                );
                let video_right = vx + video_width;
                let video_bottom = vy + video_height;
                if vy > 0.5 {
                    self.fill(
                        Rect {
                            left: 0.0,
                            top: 0.0,
                            right: layout.width,
                            bottom: vy,
                        },
                        &self.black,
                    );
                    self.fill(
                        Rect {
                            left: 0.0,
                            top: video_bottom,
                            right: layout.width,
                            bottom: layout.height,
                        },
                        &self.black,
                    );
                }
                if vx > 0.5 {
                    self.fill(
                        Rect {
                            left: 0.0,
                            top: vy,
                            right: vx,
                            bottom: video_bottom,
                        },
                        &self.black,
                    );
                    self.fill(
                        Rect {
                            left: video_right,
                            top: vy,
                            right: layout.width,
                            bottom: video_bottom,
                        },
                        &self.black,
                    );
                }
            }

            if !model.fullscreen && model.chrome_shown {
                self.draw_top_strip(model, &layout);
            }
            if model.sidebar_open {
                self.draw_sidebar(model, &layout);
            }
            if model.chrome_shown {
                self.draw_toolbar(model, &layout);
                if model.danmaku_popup {
                    self.draw_danmaku_popup(model, &layout);
                }
            }

            if !model.status.is_empty() && !model.buffering && model.error.is_none() {
                let width = 190.0 * s;
                let rect = Rect {
                    left: (layout.width - width) * 0.5,
                    top: layout.height * 0.5 - 20.0 * s,
                    right: (layout.width + width) * 0.5,
                    bottom: layout.height * 0.5 + 20.0 * s,
                };
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(rect),
                    radiusX: 20.0 * s,
                    radiusY: 20.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.panel);
                self.text_centered(&model.status, rect, &self.body_format, &self.text);
            }

            if let Some(toast) = &model.toast {
                let toast_w = (toast.text.chars().count() as f32 * 13.0 + 32.0).clamp(180.0, 520.0) * s;
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
                    if toast.error { &self.accent } else { &self.text },
                );
            }

            if let Some(error) = &model.error {
                let card = Rect {
                    left: (layout.width - 360.0 * s) * 0.5,
                    top: (layout.height * 0.5 - 78.0 * s).max(layout.top_bar.bottom + 8.0 * s),
                    right: (layout.width + 360.0 * s) * 0.5,
                    bottom: (layout.height * 0.5 + 78.0 * s).max(layout.top_bar.bottom + 8.0 * s + 156.0 * s),
                };
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(card),
                    radiusX: 12.0 * s,
                    radiusY: 12.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.toast_bg);
                self.d2d
                    .DrawRoundedRectangle(&rounded, &self.accent, 1.2 * s, None);
                self.text_centered(
                    "播放中断",
                    Rect {
                        left: card.left + 16.0 * s,
                        top: card.top + 14.0 * s,
                        right: card.right - 16.0 * s,
                        bottom: card.top + 42.0 * s,
                    },
                    &self.body_format,
                    &self.accent,
                );
                let message = ellipsize(error, 80);
                self.text_wrapped(
                    &message,
                    card.left + 20.0 * s,
                    card.top + 46.0 * s,
                    (card.width() - 40.0 * s).max(60.0),
                    48.0 * s,
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
                    if hot { &self.accent } else { &self.accent_blue },
                );
                self.text_centered("重试", retry, &self.body_format, &self.text);
            } else if model.buffering {
                let card = Rect {
                    left: (layout.width - 224.0 * s) * 0.5,
                    top: layout.height * 0.5 - 34.0 * s,
                    right: (layout.width + 224.0 * s) * 0.5,
                    bottom: layout.height * 0.5 + 34.0 * s,
                };
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(card),
                    radiusX: 10.0 * s,
                    radiusY: 10.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.toast_bg);
                self.d2d
                    .DrawRoundedRectangle(&rounded, &self.panel_border, 1.0 * s, None);
                let center = Vector2::new(card.left + 30.0 * s, (card.top + card.bottom) * 0.5);
                for index in 0..8u64 {
                    let angle = index as f32 * std::f32::consts::PI / 4.0;
                    let brush = if (self.frames + index) % 8 < 3 {
                        &self.accent
                    } else {
                        &self.track
                    };
                    self.d2d.FillEllipse(
                        &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                            point: Vector2::new(
                                center.X + angle.cos() * 9.0 * s,
                                center.Y + angle.sin() * 9.0 * s,
                            ),
                            radiusX: 2.0 * s,
                            radiusY: 2.0 * s,
                        },
                        brush,
                    );
                }
                self.text(
                    if model.status.is_empty() {
                        "正在缓冲…"
                    } else {
                        model.status.as_str()
                    },
                    card.left + 52.0 * s,
                    card.top + 22.0 * s,
                    card.width() - 64.0 * s,
                    &self.body_format,
                    &self.text,
                );
            }

            if let Some(undo) = &model.undo {
                self.draw_undo_toast(undo, &layout);
            }
            if let Some(menu) = &model.dm_menu {
                self.draw_dm_menu(model, menu, &layout);
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

    unsafe fn draw_top_strip(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        // No persistent title bar: only the floating window controls remain,
        // directly over the picture.
        for (rect, close) in [
            (layout.top_min, false),
            (layout.top_max, false),
            (layout.top_close, true),
        ] {
            let hot = model
                .pointer
                .map(|(x, y)| rect.contains(x, y))
                .unwrap_or(false);
            let brush = if hot { &self.top_hover } else { &self.top_bg };
            let rounded = D2D1_ROUNDED_RECT {
                rect: to_d2d(Rect {
                    left: rect.left + 6.0 * s,
                    top: rect.top + 8.0 * s,
                    right: rect.right - 6.0 * s,
                    bottom: rect.bottom - 8.0 * s,
                }),
                radiusX: 5.0 * s,
                radiusY: 5.0 * s,
            };
            self.d2d.FillRoundedRectangle(&rounded, brush);
            let cx = (rect.left + rect.right) * 0.5;
            let cy = (rect.top + rect.bottom) * 0.5;
            if close {
                self.line(cx - 4.0 * s, cy - 4.0 * s, cx + 4.0 * s, cy + 4.0 * s, 1.4 * s, &self.top_text);
                self.line(cx - 4.0 * s, cy + 4.0 * s, cx + 4.0 * s, cy - 4.0 * s, 1.4 * s, &self.top_text);
            } else if rect == layout.top_min {
                self.line(cx - 5.0 * s, cy, cx + 5.0 * s, cy, 1.3 * s, &self.top_text_dim);
            } else {
                self.stroke_rect(
                    Rect {
                        left: cx - 4.5 * s,
                        top: cy - 4.5 * s,
                        right: cx + 4.5 * s,
                        bottom: cy + 4.5 * s,
                    },
                    1.3 * s,
                    &self.top_text_dim,
                );
                if model.maximized {
                    self.stroke_rect(
                        Rect {
                            left: cx - 2.5 * s,
                            top: cy - 6.5 * s,
                            right: cx + 6.5 * s,
                            bottom: cy + 2.5 * s,
                        },
                        1.1 * s,
                        &self.text_dim,
                    );
                }
            }
        }
    }

    unsafe fn draw_sidebar(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        // Window-height strip: 非全屏 it is docked to the right edge, 全屏 it
        // overlays the picture. No floating rounded card.
        self.fill(layout.sidebar, &self.sidebar_panel_bg);
        self.fill(
            Rect {
                left: layout.sidebar.left,
                top: 0.0,
                right: layout.sidebar.left + s,
                bottom: layout.sidebar.bottom,
            },
            &self.sidebar_panel_line,
        );
        for (index, rect) in layout.sidebar_tabs.iter().enumerate() {
            let tab = SidebarTab::from_index(index);
            let active = model.sidebar_tab == tab;
            if active {
                self.fill(
                    Rect {
                        left: rect.left + 6.0 * s,
                        top: rect.top + 4.0 * s,
                        right: rect.right - 6.0 * s,
                        bottom: rect.bottom - 2.0 * s,
                    },
                    &self.sidebar_chip,
                );
            }
            self.text_centered(
                tab.label(),
                *rect,
                &self.tab_format,
                if active { &self.sidebar_text } else { &self.sidebar_text_dim },
            );
            if active {
                self.fill(
                    Rect {
                        left: rect.left + 14.0 * s,
                        top: rect.bottom - 2.0 * s,
                        right: rect.right - 14.0 * s,
                        bottom: rect.bottom,
                    },
                    &self.accent,
                );
            }
        }
        let close = layout.sidebar_close;
        let cx = (close.left + close.right) * 0.5;
        let cy = (close.top + close.bottom) * 0.5;
        self.line(cx - 4.0 * s, cy - 4.0 * s, cx + 4.0 * s, cy + 4.0 * s, 1.3 * s, &self.sidebar_text_dim);
        self.line(cx - 4.0 * s, cy + 4.0 * s, cx + 4.0 * s, cy - 4.0 * s, 1.3 * s, &self.sidebar_text_dim);
        self.fill(
            Rect {
                left: layout.sidebar.left,
                top: layout.sidebar_content.top - 4.0 * s,
                right: layout.sidebar.right,
                bottom: layout.sidebar_content.top - 3.0 * s,
            },
            &self.sidebar_panel_line,
        );
        match model.sidebar_tab {
            SidebarTab::Info => self.draw_info(model, layout),
            SidebarTab::Episodes => self.draw_episodes(model, layout),
            SidebarTab::Comments => self.draw_comments(model, layout),
            SidebarTab::History => self.draw_history(model, layout),
            SidebarTab::Performance => self.draw_performance(model, layout),
            SidebarTab::Settings => self.draw_settings(model, layout),
        }
    }

    unsafe fn draw_info(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        if let Some(info) = &model.info {
            let cover = Rect {
                left: content.left,
                top: content.top,
                right: content.left + 88.0 * s,
                bottom: content.top + 54.0 * s,
            };
            if let Some(image) = &info.cover {
                if !self.draw_image(image, cover) {
                    self.cover_placeholder(cover);
                }
            } else {
                self.cover_placeholder(cover);
            }
            let text_left = cover.right + 10.0 * s;
            self.text_wrapped(
                &info.title,
                text_left,
                content.top,
                (content.right - text_left).max(60.0),
                40.0 * s,
                &self.wrap_body,
                &self.sidebar_text,
            );
            let owner = if info.owner.is_empty() {
                "UP主".to_string()
            } else {
                info.owner.clone()
            };
            self.text(
                &owner,
                text_left,
                content.top + 42.0 * s,
                content.right - text_left,
                &self.small_format,
                &self.accent,
            );
            let date = format_date(info.pubdate);
            let meta = format!(
                "{} 播放 · {} 弹幕",
                compact_number(info.views),
                model.live_danmaku_count
            );
            self.text(
                &meta,
                text_left,
                content.top + 60.0 * s,
                content.right - text_left,
                &self.small_format,
                &self.sidebar_text_dim,
            );
            if !date.is_empty() {
                self.text(
                    &date,
                    text_left,
                    content.top + 76.0 * s,
                    content.right - text_left,
                    &self.hint_format,
                    &self.sidebar_text_faint,
                );
            }
            self.text(
                "互动",
                content.left,
                content.top + 126.0 * s,
                120.0 * s,
                &self.small_format,
                &self.sidebar_text_faint,
            );
            let labels = [
                if model.liked { "已点赞" } else { "点赞" },
                if model.coined { "已投币" } else { "投币" },
                if model.favorited { "已收藏" } else { "收藏" },
            ];
            let active = [model.liked, model.coined, model.favorited];
            for (index, rect) in layout.info_actions.iter().enumerate() {
                let hot = model
                    .pointer
                    .map(|(x, y)| rect.contains(x, y))
                    .unwrap_or(false);
                let brush = if active[index] {
                    &self.accent
                } else if hot {
                    &self.sidebar_hover
                } else {
                    &self.sidebar_chip
                };
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(*rect),
                    radiusX: 7.0 * s,
                    radiusY: 7.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, brush);
                self.text_centered(
                    labels[index],
                    *rect,
                    &self.small_format,
                    if active[index] { &self.text } else { &self.sidebar_text_dim },
                );
            }
            self.text(
                "简介",
                content.left,
                content.top + 196.0 * s,
                80.0 * s,
                &self.small_format,
                &self.sidebar_text_faint,
            );
            let description = if info.description.trim().is_empty() {
                "暂无简介"
            } else {
                &info.description
            };
            self.text_wrapped(
                description,
                content.left,
                content.top + 216.0 * s,
                content.width(),
                120.0 * s,
                &self.wrap_small,
                &self.sidebar_text_dim,
            );
        } else {
            self.text(
                "正在加载视频信息…",
                content.left,
                content.top,
                content.width(),
                &self.body_format,
                &self.sidebar_text_dim,
            );
        }
    }

    unsafe fn cover_placeholder(&self, rect: Rect) {
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(rect),
            radiusX: 6.0 * self.layout.scale,
            radiusY: 6.0 * self.layout.scale,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_chip);
        self.text_centered("O11A1", rect, &self.small_format, &self.sidebar_text_faint);
    }

    unsafe fn draw_episodes(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
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
                layout.sidebar_content.left,
                layout.sidebar_content.top,
                layout.sidebar_content.width(),
                &self.body_format,
                &self.sidebar_text_dim,
            );
            return;
        }
        let current = if model.current_page < pages.len() {
            model.current_page
        } else {
            model.info.as_ref().map(|info| info.current_page).unwrap_or(0)
        };
        for (index, page) in pages.iter().enumerate() {
            let rect = layout.episode_row(index, model.sidebar_scroll);
            if rect.bottom < layout.sidebar_content.top || rect.top > layout.sidebar_content.bottom {
                continue;
            }
            if index == current {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(rect),
                    radiusX: 6.0 * s,
                    radiusY: 6.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_chip);
            }
            let label = format!("P{}  {}", index + 1, page.title);
            self.text(
                &ellipsize(&label, 16),
                rect.left + 10.0 * s,
                rect.top + 10.0 * s,
                rect.width() - 70.0 * s,
                &self.small_format,
                if index == current { &self.sidebar_text } else { &self.sidebar_text_dim },
            );
            self.text_right(
                &format_time(page.duration.saturating_mul(1000)),
                rect.right - 8.0 * s,
                rect.top + 12.0 * s,
                52.0 * s,
                &self.hint_format,
                &self.sidebar_text_faint,
            );
        }
    }

    unsafe fn draw_comments(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        self.text(
            &format!("评论 {}", model.comments_total.max(0)),
            content.left,
            content.top + 8.0 * s,
            130.0 * s,
            &self.small_format,
            &self.sidebar_text_dim,
        );
        for (index, rect) in layout.comments_sort.iter().enumerate() {
            let hot = index == 0;
            let active = hot == model.comments_hot;
            if active {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(*rect),
                    radiusX: 13.0 * s,
                    radiusY: 13.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_chip);
            }
            self.text_centered(
                if hot { "热门" } else { "时间" },
                *rect,
                &self.hint_format,
                if active { &self.sidebar_text } else { &self.sidebar_text_dim },
            );
        }
        if model.comments.is_empty() {
            self.text(
                if model.comments_loading {
                    "正在加载评论…"
                } else {
                    "暂无评论"
                },
                content.left,
                content.top + 84.0 * s,
                content.width(),
                &self.body_format,
                &self.sidebar_text_dim,
            );
            return;
        }
        for (index, comment) in model.comments.iter().enumerate() {
            let rect = layout.comment_row(index, model.sidebar_scroll);
            if rect.bottom < content.top || rect.top > content.bottom {
                continue;
            }
            let avatar = Rect {
                left: rect.left,
                top: rect.top + 4.0 * s,
                right: rect.left + 32.0 * s,
                bottom: rect.top + 36.0 * s,
            };
            let image_drawn = comment
                .avatar
                .as_ref()
                .map(|image| self.draw_image_circular(image, avatar))
                .unwrap_or(false);
            if !image_drawn {
                self.d2d.FillEllipse(
                    &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                        point: Vector2::new(
                            (avatar.left + avatar.right) * 0.5,
                            (avatar.top + avatar.bottom) * 0.5,
                        ),
                        radiusX: 16.0 * s,
                        radiusY: 16.0 * s,
                    },
                    &self.sidebar_avatar_bg,
                );
                let initial: String = comment.user.chars().take(1).collect();
                self.text_centered(&initial, avatar, &self.small_format, &self.sidebar_text);
            }
            let text_left = avatar.right + 8.0 * s;
            self.text(
                &ellipsize(&comment.user, 18),
                text_left,
                rect.top + 2.0 * s,
                rect.width() - 50.0 * s,
                &self.hint_format,
                &self.sidebar_text_dim,
            );
            let message = ellipsize(&comment.message, 34);
            self.text_wrapped(
                &message,
                text_left,
                rect.top + 19.0 * s,
                (rect.right - text_left - 8.0 * s).max(40.0),
                34.0 * s,
                &self.wrap_small,
                &self.sidebar_text,
            );
            self.text_right(
                &compact_number(comment.likes),
                rect.right,
                rect.top + 2.0 * s,
                44.0 * s,
                &self.hint_format,
                &self.sidebar_text_faint,
            );
            self.fill(
                Rect {
                    left: rect.left,
                    top: rect.bottom - 1.0 * s,
                    right: rect.right,
                    bottom: rect.bottom,
                },
                &self.sidebar_panel_line,
            );
        }
        if model.comments_loading {
            self.text_centered(
                "加载中…",
                layout.comments_more,
                &self.hint_format,
                &self.sidebar_text_dim,
            );
        } else if !model.comments_end {
            let hot = model
                .pointer
                .map(|(x, y)| layout.comments_more.contains(x, y))
                .unwrap_or(false);
            let brush = if hot { &self.sidebar_hover } else { &self.sidebar_chip };
            let rounded = D2D1_ROUNDED_RECT {
                rect: to_d2d(layout.comments_more),
                radiusX: 7.0 * s,
                radiusY: 7.0 * s,
            };
            self.d2d.FillRoundedRectangle(&rounded, brush);
            self.text_centered(
                "加载更多",
                layout.comments_more,
                &self.small_format,
                &self.sidebar_text_dim,
            );
        } else {
            self.text_centered(
                "没有更多评论",
                layout.comments_more,
                &self.hint_format,
                &self.sidebar_text_faint,
            );
        }
    }

    unsafe fn draw_history(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        self.text(
            "弹幕历史",
            content.left,
            content.top + 2.0 * s,
            130.0 * s,
            &self.body_format,
            &self.sidebar_text,
        );
        self.text(
            &format!("实时 {} 条", model.live_danmaku_count),
            content.left + 132.0 * s,
            content.top + 4.0 * s,
            (content.width() - 132.0 * s).max(80.0),
            &self.hint_format,
            &self.sidebar_text_faint,
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
        self.text(
            "实时弹幕",
            live.left + 10.0 * s,
            live.top + 9.0 * s,
            live.width() - 20.0 * s,
            &self.small_format,
            if live_active {
                &self.sidebar_text
            } else {
                &self.sidebar_text_dim
            },
        );

        if model.history_dates_loading {
            self.text(
                "正在获取历史弹幕日期…",
                content.left,
                live.bottom + 14.0 * s,
                content.width(),
                &self.small_format,
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
                &self.wrap_small,
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
                &self.small_format,
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
                &self.small_format,
                &self.accent,
            );
        } else if let Some(error) = &model.history_error {
            self.text_wrapped(
                &format!("历史弹幕加载失败：{}", ellipsize(error, 76)),
                content.left,
                live.bottom + 12.0 * s,
                content.width(),
                46.0 * s,
                &self.wrap_small,
                &self.accent,
            );
        }
        for (index, date) in model.history_dates.iter().enumerate() {
            let rect = layout.history_row(index, model.sidebar_scroll);
            if rect.bottom < layout.sidebar_content.top
                || rect.top > layout.sidebar_content.bottom
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
            self.text(
                date,
                rect.left + 10.0 * s,
                rect.top + 8.0 * s,
                rect.width() - 20.0 * s,
                &self.small_format,
                if active {
                    &self.sidebar_text
                } else {
                    &self.sidebar_text_dim
                },
            );
            if active {
                self.text_right(
                    &format!("{} 条", model.history_count),
                    rect.right - 8.0 * s,
                    rect.top + 9.0 * s,
                    70.0 * s,
                    &self.hint_format,
                    &self.accent,
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
            &self.hint_format,
            &self.sidebar_text_faint,
        );
    }

    unsafe fn draw_performance(&self, model: &UiModel, layout: &Layout) {        let s = layout.scale;
        let content = layout.sidebar_content;
        let lines = [
            format!("fps        {:.1}   源 {:.1}", model.fps, model.source_fps),
            format!("位置       {}", crate::format::format_time_ms(model.position_ms)),
            format!("音频时钟   {}", crate::format::format_time_ms(model.audio_ms)),
            format!("队列/丢帧  {} / {}", model.queue_len, model.dropped),
            format!("生成帧     {}", model.generated),
            format!("混合       {:.2} ms", model.blend_us as f32 / 1000.0),
            format!("分辨率     {}×{}", model.source_size.0, model.source_size.1),
            format!("插帧       {}", model.interp.label()),
            format!("画质增强   {}", model.enhance.label()),
            format!("弹幕       {}/{}", model.danmaku_total, model.danmaku_active),
            format!("UI         {:.1} ms", model.ui_ms),
            format!("渲染器     {}", model.renderer),
        ];
        for (index, line) in lines.iter().enumerate() {
            self.text(
                line,
                content.left,
                content.top + index as f32 * 23.0 * s,
                content.width(),
                &self.small_format,
                if index == 0 { &self.accent } else { &self.sidebar_text_dim },
            );
        }
    }

    unsafe fn draw_settings(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let content = layout.sidebar_content;
        let quality_value = if model.codec.is_empty() {
            model.quality_label.clone()
        } else {
            format!("{} · {}", model.quality_label, model.codec)
        };
        let rows = [
            ("画质", quality_value, true),
            ("音轨", model.audio_label.clone(), true),
            ("速度", format!("{:.2}×", model.speed), true),
            ("比例", model.aspect.label().to_string(), true),
            ("亮度", format!("{:.0}%", model.brightness * 100.0), true),
            ("插帧", model.interp.label().to_string(), true),
            ("画质增强", model.enhance.label().to_string(), true),
            ("弹幕", if model.danmaku_visible { "开" } else { "关" }.to_string(), true),
            ("弹幕设置", "打开".to_string(), true),
            ("置顶", if model.ontop { "开" } else { "关" }.to_string(), true),
            ("仅音频", if model.audio_only { "开" } else { "关" }.to_string(), true),
            ("睡眠定时", model.sleep_label.clone(), true),
            ("快捷键", model.keys_label.clone(), false),
        ];
        for (index, (label, value, editable)) in rows.iter().enumerate() {
            let rect = layout.settings_row(index, model.sidebar_scroll);
            if rect.bottom < content.top || rect.top > content.bottom {
                continue;
            }
            let editable = *editable;
            let hot = editable
                && model
                    .pointer
                    .map(|(x, y)| rect.contains(x, y))
                    .unwrap_or(false);
            if hot {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(rect),
                    radiusX: 6.0 * s,
                    radiusY: 6.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_hover);
            }
            self.text(
                label,
                rect.left + 8.0 * s,
                rect.top + 10.0 * s,
                rect.width() * 0.38,
                &self.small_format,
                &self.sidebar_text_dim,
            );
            let right_width = rect.width() * 0.60;
            self.text_right(
                value,
                rect.right - 8.0 * s,
                rect.top + 10.0 * s,
                right_width,
                &self.small_format,
                if editable { &self.sidebar_text } else { &self.sidebar_text_faint },
            );
            self.fill(
                Rect {
                    left: rect.left,
                    top: rect.bottom - 1.0 * s,
                    right: rect.right,
                    bottom: rect.bottom,
                },
                &self.sidebar_panel_line,
            );
        }
        if let Some(menu) = model.settings_menu {
            self.draw_settings_menu(model, layout, menu);
        }
    }

    unsafe fn draw_settings_menu(&self, model: &UiModel, layout: &Layout, menu: SettingsMenu) {
        let s = layout.scale;
        let content = layout.settings_menu;
        let (title, count) = match menu {
            SettingsMenu::Quality => ("画质", model.quality_rows.len()),
            SettingsMenu::Audio => ("音轨", model.audio_rows.len()),
        };
        let row_h = 34.0 * s;
        let used = 34.0 * s + row_h * count as f32 + 8.0 * s;
        let panel = Rect {
            left: content.left,
            top: content.top,
            right: content.right,
            bottom: (content.top + used).min(content.bottom),
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(panel),
            radiusX: 8.0 * s,
            radiusY: 8.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_panel_bg);
        self.d2d
            .DrawRoundedRectangle(&rounded, &self.sidebar_panel_line, 1.0 * s, None);
        self.text(
            title,
            panel.left + 10.0 * s,
            panel.top + 8.0 * s,
            120.0 * s,
            &self.body_format,
            &self.sidebar_text,
        );
        self.text_right(
            "Esc 关闭",
            panel.right - 10.0 * s,
            panel.top + 10.0 * s,
            90.0 * s,
            &self.hint_format,
            &self.sidebar_text_faint,
        );
        for index in 0..count {
            let rect = layout.settings_menu_row(index, 0.0);
            if rect.top > panel.bottom {
                break;
            }
            let hot = model
                .pointer
                .map(|(x, y)| rect.contains(x, y))
                .unwrap_or(false);
            match menu {
                SettingsMenu::Quality => {
                    let row = &model.quality_rows[index];
                    if row.current {
                        let rounded = D2D1_ROUNDED_RECT {
                            rect: to_d2d(rect),
                            radiusX: 5.0 * s,
                            radiusY: 5.0 * s,
                        };
                        self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_chip);
                    } else if hot && !row.locked {
                        let rounded = D2D1_ROUNDED_RECT {
                            rect: to_d2d(rect),
                            radiusX: 5.0 * s,
                            radiusY: 5.0 * s,
                        };
                        self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_hover);
                    }
                    let brush = if row.locked {
                        &self.sidebar_text_faint
                    } else if row.current {
                        &self.accent
                    } else {
                        &self.sidebar_text
                    };
                    self.text(
                        &row.label,
                        rect.left + 16.0 * s,
                        rect.top + 8.0 * s,
                        rect.width() * 0.62,
                        &self.small_format,
                        brush,
                    );
                    self.text_right(
                        &row.detail,
                        rect.right - 10.0 * s,
                        rect.top + 9.0 * s,
                        rect.width() * 0.42,
                        &self.hint_format,
                        if row.locked { &self.sidebar_text_faint } else { &self.sidebar_text_dim },
                    );
                }
                SettingsMenu::Audio => {
                    let row = &model.audio_rows[index];
                    if row.current {
                        let rounded = D2D1_ROUNDED_RECT {
                            rect: to_d2d(rect),
                            radiusX: 5.0 * s,
                            radiusY: 5.0 * s,
                        };
                        self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_chip);
                    } else if hot {
                        let rounded = D2D1_ROUNDED_RECT {
                            rect: to_d2d(rect),
                            radiusX: 5.0 * s,
                            radiusY: 5.0 * s,
                        };
                        self.d2d.FillRoundedRectangle(&rounded, &self.sidebar_hover);
                    }
                    self.text(
                        &row.label,
                        rect.left + 16.0 * s,
                        rect.top + 8.0 * s,
                        rect.width() * 0.68,
                        &self.small_format,
                        if row.current { &self.accent } else { &self.sidebar_text },
                    );
                    self.text_right(
                        &row.detail,
                        rect.right - 10.0 * s,
                        rect.top + 9.0 * s,
                        rect.width() * 0.32,
                        &self.hint_format,
                        &self.sidebar_text_dim,
                    );
                }
            }
        }
    }

    unsafe fn draw_toolbar(&mut self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        // Progress hairline above the pill.
        self.fill(layout.track, &self.track);
        let fraction = if model.duration_ms > 0 {
            (model.drag_preview.unwrap_or(model.position_ms) as f32 / model.duration_ms as f32)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let filled_right = layout.track.left + layout.track.width() * fraction;
        self.fill(
            Rect {
                left: layout.track.left,
                top: layout.track.top,
                right: filled_right,
                bottom: layout.track.bottom,
            },
            &self.accent,
        );
        let pointer_on_track = model
            .pointer
            .map(|(x, y)| layout.progress_hit(x, y))
            .unwrap_or(false);
        if pointer_on_track || model.drag_preview.is_some() {
            self.d2d.FillEllipse(
                &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                    point: Vector2::new(filled_right, layout.track.top + 1.5 * s),
                    radiusX: 5.0 * s,
                    radiusY: 5.0 * s,
                },
                &self.text,
            );
        }

        let pill = D2D1_ROUNDED_RECT {
            rect: to_d2d(layout.panel),
            radiusX: 12.0 * s,
            radiusY: 12.0 * s,
        };
        self.d2d.FillRoundedRectangle(&pill, &self.chrome);
        self.d2d
            .DrawRoundedRectangle(&pill, &self.panel_border, 1.0 * s, None);

        // Play / pause.
        let center = Vector2::new(
            (layout.play.left + layout.play.right) * 0.5,
            (layout.play.top + layout.play.bottom) * 0.5,
        );
        self.d2d.FillEllipse(
            &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                point: center,
                radiusX: 16.0 * s,
                radiusY: 16.0 * s,
            },
            &self.accent_blue,
        );
        if model.playing {
            for sign in [-1.0f32, 1.0] {
                self.fill(
                    Rect {
                        left: center.X + sign * 4.0 * s - 2.0 * s,
                        top: center.Y - 6.0 * s,
                        right: center.X + sign * 4.0 * s + 2.0 * s,
                        bottom: center.Y + 6.0 * s,
                    },
                    &self.top_text,
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
                if let Ok(brush) = self.text.cast::<ID2D1Brush>() {
                    let _ = self
                        .render_target
                        .FillGeometry(&geometry, &brush, None::<&ID2D1Brush>);
                }
            }
            self.render_target.SetTransform(&Matrix3x2::identity());
        }
        let time = format!(
            "{} / {}",
            format_time(model.drag_preview.unwrap_or(model.position_ms)),
            format_time(model.duration_ms)
        );
        self.text(
            &time,
            layout.play.right + 8.0 * s,
            center.Y - 8.0 * s,
            96.0 * s,
            &self.small_format,
            &self.text,
        );

        // Storyboard / time bubble above the progress line. The image comes
        // from the background videoshot worker; a failed fetch leaves the
        // time-only bubble (no error dialog, exactly like the old player).
        if let Some(preview_ms) = model.drag_preview.or(model.hover_preview) {
            let fraction = if model.duration_ms > 0 {
                (preview_ms as f32 / model.duration_ms as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let anchor_x = layout.track.left + layout.track.width() * fraction;
            let bubble_w = 68.0 * s;
            let bubble = Rect {
                left: (anchor_x - bubble_w * 0.5)
                    .clamp(layout.track.left, layout.track.right - bubble_w),
                top: layout.track.top - 28.0 * s,
                right: 0.0,
                bottom: layout.track.top - 8.0 * s,
            };
            let bubble = Rect {
                right: bubble.left + bubble_w,
                ..bubble
            };
            let bubble_rounded = D2D1_ROUNDED_RECT {
                rect: to_d2d(bubble),
                radiusX: 6.0 * s,
                radiusY: 6.0 * s,
            };
            self.d2d.FillRoundedRectangle(&bubble_rounded, &self.chip);
            self.text_centered(
                &format_time(preview_ms),
                bubble,
                &self.hint_format,
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
    }

    unsafe fn draw_toolbar_buttons(&self, model: &UiModel, layout: &Layout) {
        let s = layout.scale;
        let buttons = layout.toolbar_buttons;
        for (index, rect) in buttons.iter().enumerate() {
            let hot = model
                .pointer
                .map(|(x, y)| rect.contains(x, y))
                .unwrap_or(false);
            if hot {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: to_d2d(*rect),
                    radiusX: 7.0 * s,
                    radiusY: 7.0 * s,
                };
                self.d2d.FillRoundedRectangle(&rounded, &self.hover);
            }
            let cx = (rect.left + rect.right) * 0.5;
            let cy = (rect.top + rect.bottom) * 0.5;
            match index {
                TB_VOLUME => self.icon_volume(cx, cy, s, model.muted),
                TB_DANMAKU => self.icon_danmaku(
                    cx,
                    cy,
                    s,
                    if model.danmaku_visible { &self.text } else { &self.text_faint },
                ),
                TB_DANMAKU_SETTINGS => self.icon_gear(cx, cy, s, &self.text_dim),
                TB_SPEED => self.text_centered(
                    &format!("{:.2}×", model.speed),
                    *rect,
                    &self.hint_format,
                    &self.text_dim,
                ),
                TB_ASPECT => {
                    self.icon_aspect(cx, cy, s);
                }
                TB_SHOT => self.icon_camera(cx, cy, s),
                TB_MINI => self.icon_mini(cx, cy, s),
                TB_FULLSCREEN => self.icon_fullscreen(cx, cy, s),
                TB_PLAYLIST => self.icon_playlist(cx, cy, s),
                TB_SIDEBAR => self.icon_sidebar(
                    cx,
                    cy,
                    s,
                    if model.sidebar_open { &self.accent } else { &self.text_dim },
                ),
                TB_SETTINGS => self.icon_gear(cx, cy, s, &self.text_dim),
                _ => {}
            }
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
            popup.left + 14.0 * s,
            popup.top + 10.0 * s,
            160.0 * s,
            &self.body_format,
            &self.text,
        );
        let close = layout.popup_close;
        let cx = (close.left + close.right) * 0.5;
        let cy = (close.top + close.bottom) * 0.5;
        self.line(cx - 4.0 * s, cy - 4.0 * s, cx + 4.0 * s, cy + 4.0 * s, 1.3 * s, &self.text_dim);
        self.line(cx - 4.0 * s, cy + 4.0 * s, cx + 4.0 * s, cy - 4.0 * s, 1.3 * s, &self.text_dim);

        self.popup_slider(
            "透明度",
            layout.popup_opacity,
            model.danmaku_opacity as f32 / 100.0,
            &format!("{}%", model.danmaku_opacity),
            layout,
        );
        self.text(
            "字号",
            popup.left + 14.0 * s,
            popup.top + 82.0 * s,
            70.0 * s,
            &self.small_format,
            &self.text_dim,
        );
        for (index, rect) in layout.popup_font.iter().enumerate() {
            let active = model.danmaku_font as usize == index;
            self.popup_choice(*rect, &["小", "标准", "大"][index], active, layout);
        }
        self.popup_slider(
            "速度",
            layout.popup_speed,
            (model.danmaku_speed as f32 - 50.0) / 150.0,
            &format!("{}%", model.danmaku_speed),
            layout,
        );
        self.text(
            "区域",
            popup.left + 14.0 * s,
            popup.top + 164.0 * s,
            70.0 * s,
            &self.small_format,
            &self.text_dim,
        );
        for (index, rect) in layout.popup_area.iter().enumerate() {
            let value = [25, 50, 75, 100][index];
            self.popup_choice(*rect, &format!("{value}%"), model.danmaku_area == value, layout);
        }
        self.text(
            "屏蔽",
            popup.left + 14.0 * s,
            popup.top + 202.0 * s,
            70.0 * s,
            &self.small_format,
            &self.text_dim,
        );
        let block_labels = ["滚动", "顶部", "底部"];
        let block_active = [model.block_scroll, model.block_top, model.block_bottom];
        for (index, rect) in layout.popup_blocks.iter().enumerate() {
            self.popup_choice(*rect, block_labels[index], block_active[index], layout);
        }
        self.text(
            "分布密度",
            popup.left + 14.0 * s,
            popup.top + 240.0 * s,
            74.0 * s,
            &self.small_format,
            &self.text_dim,
        );
        self.popup_choice(
            layout.popup_density,
            if model.density_on { "开" } else { "关" },
            model.density_on,
            layout,
        );
        // 发送 box: the engine owns the text + WBI-signed dm/post call.
        let send = Rect {
            left: layout.popup_send.left,
            top: layout.popup_send.top,
            right: layout.popup_send.right - 58.0 * s,
            bottom: layout.popup_send.bottom,
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(send),
            radiusX: 7.0 * s,
            radiusY: 7.0 * s,
        };
        self.d2d.FillRoundedRectangle(&rounded, &self.chip);
        let shown = if model.danmaku_input.is_empty() {
            if model.danmaku_input_focus {
                "输入弹幕，回车发送"
            } else {
                "发个弹幕…"
            }
        } else {
            model.danmaku_input.as_str()
        };
        self.text(
            shown,
            send.left + 10.0 * s,
            send.top + 8.0 * s,
            send.width(),
            &self.hint_format,
            if model.danmaku_input.is_empty() {
                &self.text_faint
            } else {
                &self.text
            },
        );
        let send_btn = Rect {
            left: layout.popup_send.right - 52.0 * s,
            top: layout.popup_send.top,
            right: layout.popup_send.right,
            bottom: layout.popup_send.bottom,
        };
        let rounded = D2D1_ROUNDED_RECT {
            rect: to_d2d(send_btn),
            radiusX: 7.0 * s,
            radiusY: 7.0 * s,
        };
        self.d2d.FillRoundedRectangle(
            &rounded,
            if model.danmaku_input.trim().is_empty() {
                &self.chip
            } else {
                &self.accent
            },
        );
        self.text_centered(
            "发送",
            send_btn,
            &self.hint_format,
            if model.danmaku_input.trim().is_empty() {
                &self.text_faint
            } else {
                &self.text
            },
        );
    }

    unsafe fn popup_slider(
        &self,
        label: &str,
        rect: Rect,
        fraction: f32,
        value: &str,
        layout: &Layout,
    ) {
        let s = layout.scale;
        self.text(
            label,
            layout.popup.left + 14.0 * s,
            rect.top - 7.0 * s,
            70.0 * s,
            &self.small_format,
            &self.text_dim,
        );
        let track = Rect {
            left: layout.popup.left + 90.0 * s,
            top: rect.top + 3.0 * s,
            right: layout.popup.right - 54.0 * s,
            bottom: rect.top + 5.0 * s,
        };
        self.fill(
            Rect {
                left: track.left,
                top: track.top - 1.0 * s,
                right: track.right,
                bottom: track.bottom + 1.0 * s,
            },
            &self.track,
        );
        let right = track.left + track.width() * fraction.clamp(0.0, 1.0);
        self.fill(
            Rect {
                left: track.left,
                top: track.top - 1.0 * s,
                right,
                bottom: track.bottom + 1.0 * s,
            },
            &self.accent,
        );
        self.d2d.FillEllipse(
            &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                point: Vector2::new(right, (track.top + track.bottom) * 0.5),
                radiusX: 5.0 * s,
                radiusY: 5.0 * s,
            },
            &self.text,
        );
        self.text_right(
            value,
            layout.popup.right - 12.0 * s,
            rect.top - 8.0 * s,
            44.0 * s,
            &self.hint_format,
            &self.text_dim,
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
            &self.hint_format,
            if active { &self.text } else { &self.text_dim },
        );
    }

    unsafe fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, brush: &ID2D1SolidColorBrush) {
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

    unsafe fn icon_volume(&self, cx: f32, cy: f32, s: f32, muted: bool) {
        let brush = if muted { &self.text_faint } else { &self.text_dim };
        self.fill(
            Rect {
                left: cx - 8.0 * s,
                top: cy - 3.0 * s,
                right: cx - 3.0 * s,
                bottom: cy + 3.0 * s,
            },
            brush,
        );
        self.line(cx - 3.0 * s, cy - 3.0 * s, cx + 3.0 * s, cy - 7.0 * s, 1.4 * s, brush);
        self.line(cx - 3.0 * s, cy + 3.0 * s, cx + 3.0 * s, cy + 7.0 * s, 1.4 * s, brush);
        self.line(cx + 3.0 * s, cy - 7.0 * s, cx + 3.0 * s, cy + 7.0 * s, 1.4 * s, brush);
        if muted {
            self.line(cx + 6.0 * s, cy - 4.0 * s, cx + 10.0 * s, cy + 4.0 * s, 1.3 * s, &self.accent);
            self.line(cx + 10.0 * s, cy - 4.0 * s, cx + 6.0 * s, cy + 4.0 * s, 1.3 * s, &self.accent);
        } else {
            self.line(cx + 5.0 * s, cy - 3.0 * s, cx + 8.0 * s, cy, 1.2 * s, brush);
            self.line(cx + 8.0 * s, cy, cx + 5.0 * s, cy + 3.0 * s, 1.2 * s, brush);
        }
    }

    unsafe fn icon_danmaku(&self, cx: f32, cy: f32, s: f32, brush: &ID2D1SolidColorBrush) {
        self.stroke_rect(
            Rect {
                left: cx - 10.0 * s,
                top: cy - 7.0 * s,
                right: cx + 10.0 * s,
                bottom: cy + 5.0 * s,
            },
            1.3 * s,
            brush,
        );
        self.fill(
            Rect {
                left: cx - 10.0 * s,
                top: cy + 3.0 * s,
                right: cx - 5.0 * s,
                bottom: cy + 8.0 * s,
            },
            brush,
        );
        self.line(cx - 6.0 * s, cy - 2.0 * s, cx + 6.0 * s, cy - 2.0 * s, 1.2 * s, brush);
        self.line(cx - 6.0 * s, cy + 1.0 * s, cx + 3.0 * s, cy + 1.0 * s, 1.2 * s, brush);
    }

    unsafe fn icon_gear(&self, cx: f32, cy: f32, s: f32, brush: &ID2D1SolidColorBrush) {
        for index in 0..6 {
            let angle = index as f32 * std::f32::consts::PI / 3.0;
            self.line(
                cx + angle.cos() * 6.0 * s,
                cy + angle.sin() * 6.0 * s,
                cx + angle.cos() * 9.0 * s,
                cy + angle.sin() * 9.0 * s,
                1.8 * s,
                brush,
            );
        }
        unsafe {
            let _ = self.render_target.DrawEllipse(
                &windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                    point: Vector2::new(cx, cy),
                    radiusX: 5.5 * s,
                    radiusY: 5.5 * s,
                },
                brush,
                1.5 * s,
                None,
            );
        }
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
        self.line(cx + 2.0 * s, cy - 7.0 * s, cx + 2.0 * s, cy + 7.0 * s, 1.2 * s, brush);
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
            maskAntialiasMode: windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
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

    unsafe fn draw_image_contained(
        &self,
        bitmap: &ID2D1Bitmap1,
        dest: Rect,
        inset: f32,
    ) -> bool {
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

    fn fill(&self, rect: Rect, brush: &ID2D1SolidColorBrush) {
        unsafe {
            self.render_target.FillRectangle(&to_d2d(rect), brush);
        }
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
                let mut style = *style;
                style.baseline += 2.0;
                unsafe {
                    gdi.draw(
                        &self.render_target,
                        text,
                        style,
                        rect.left,
                        rect.top,
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
            let y = snap_layout_y(&layout, rect.top + 2.0);
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

/// Load the old rounded UI font privately (same asset as the main window).
/// Falls back to Microsoft YaHei UI when the asset is missing.
pub(crate) fn player_font_family() -> PCWSTR {
    let _ = chiron_loaded();
    w!("Chiron GoRound TC")
}

/// True when both Chiron faces were loaded into this process.
pub(crate) fn player_font_available() -> bool {
    chiron_loaded()
}

fn chiron_loaded() -> bool {
    static LOADED: OnceLock<bool> = OnceLock::new();
    *LOADED.get_or_init(|| {
        let mut dirs = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                dirs.push(dir.join("assets/fonts"));
                dirs.push(dir.join("../assets/fonts"));
            }
        }
        dirs.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fonts"));
        dirs.push(std::path::PathBuf::from("assets/fonts"));
        let files = ["ChironGoRoundTC-Regular.ttf", "ChironGoRoundTC-SemiBold.ttf"];
        for dir in dirs {
            if !dir.join(files[0]).exists() {
                continue;
            }
            let mut loaded = 0;
            for file in files {
                let path = dir.join(file);
                if !path.exists() {
                    continue;
                }
                let wide: Vec<u16> = path
                    .as_os_str()
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                loaded += unsafe {
                    AddFontResourceExW(
                        PCWSTR(wide.as_ptr()),
                        FONT_RESOURCE_CHARACTERISTICS(FR_PRIVATE.0 | FR_NOT_ENUM.0),
                        None,
                    )
                };
            }
            if loaded >= files.len() as i32 {
                return true;
            }
        }
        false
    })
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

fn format_date(unix: i64) -> String {
    if unix <= 0 {
        return String::new();
    }
    let days = unix / 86_400;
    let (year, month, day) = o11a1::civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
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
    (overlay.width, overlay.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_tab_indices_include_history() {
        assert_eq!(SidebarTab::from_index(0), SidebarTab::Info);
        assert_eq!(SidebarTab::from_index(3), SidebarTab::History);
        assert_eq!(SidebarTab::from_index(4), SidebarTab::Performance);
        assert_eq!(SidebarTab::from_index(5), SidebarTab::Settings);
        assert_eq!(SidebarTab::History.label(), "历史");
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
            assert!(menu.contains(
                (row.left + row.right) * 0.5,
                (row.top + row.bottom) * 0.5
            ));
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
            assert!(report.contains(
                (row.left + row.right) * 0.5,
                (row.top + row.bottom) * 0.5
            ));
        }
    }

    #[test]
    fn layout_has_six_tabs_and_an_undo_button_above_the_toolbar() {
        let layout = Layout::new(1280.0, 720.0, 1.0, true, false);
        assert_eq!(layout.sidebar_tabs.len(), 6);
        assert!(layout.history_live.top > layout.sidebar_content.top);
        assert!(layout.undo_toast.bottom <= layout.panel.top);
        assert!(layout.undo_button.right <= layout.undo_toast.right);
        assert!(layout.undo_button.left >= layout.undo_toast.left);
        // The 历史 tab's live row is above its first date row.
        assert!(layout.history_row(0, 0.0).top > layout.history_live.bottom);
    }
}

