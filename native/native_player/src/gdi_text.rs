//! Hinted grayscale text, coloured by Direct2D.
//!
//! Chiron GoRound TC ships no TrueType instructions
//! (`maxp.maxSizeOfInstructions == 0`, no `fpgm`/`cvt`, no glyph programs), so
//! outline hinting has nothing to execute and 11-14px glyphs stay soft. GDI's
//! `ANTIALIASED_QUALITY` rasteriser synthesises grid-fitting even for such
//! fonts. Measurement and line metrics are pure GDI (`OUTLINETEXTMETRIC`,
//! because Chiron's `usWinAscent` is 1810/1000 and `tmAscent` is not a usable
//! baseline). The mask itself is rasterised at 5x and box-averaged 5x5 back to
//! 1x (`SUPERSAMPLE`), then a 3x3 unsharp mask (`SHARPEN`) restores edge
//! contrast before tinting. Layout (box, alignment, trimming, scrolling,
//! layers) stays at 1x with the caller.

use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT, D2D_SIZE_U, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1Bitmap, ID2D1RenderTarget, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
    D2D1_BITMAP_PROPERTIES,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
    GetOutlineTextMetricsW, GetTextMetricsW, SelectObject, SetBkColor, SetBkMode, SetTextColor,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, DRAW_TEXT_FORMAT, DT_CALCRECT, DT_CENTER, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, DT_TOP, DT_WORDBREAK, FF_DONTCARE, HDC, HFONT, HGDIOBJ, OUTLINETEXTMETRICW,
    OUT_DEFAULT_PRECIS, TEXTMETRICW, TRANSPARENT, ANTIALIASED_QUALITY,
};

const MAX_MASKS: usize = 2048;
/// Vertical padding around the layout box so hinted glyphs can spill a pixel.
const PAD: i32 = 4;
/// Render the mask at 5x, then box-average 5x5 blocks back to 1x.
pub const SUPERSAMPLE: u32 = 5;
/// Unsharp-mask strength applied to the 1x coverage after downsampling.
pub const SHARPEN: f32 = 0.3;

/// One rasterisation size/weight plus the layout baseline the caller wants.
#[derive(Clone, Copy)]
pub struct Style {
    pub size: f32,
    pub weight: u16,
    /// Distance from the top of the box to the first baseline.
    pub baseline: f32,
    /// Baseline-to-baseline distance the surrounding layout uses.
    pub line_height: f32,
}

struct Mask {
    bitmap: ID2D1Bitmap,
    w: f32,
    h: f32,
}

struct FontEntry {
    font: HFONT,
    /// GDI cell ascent, used to place the DrawTextW rectangle.
    ascent: i32,
    /// Typographic ascent / line gap from OUTLINETEXTMETRIC, scaled by the
    /// requested size. These are the metrics DWrite used to expose, so boxes
    /// designed around them keep their baseline.
    baseline_ratio: f32,
    height_ratio: f32,
}

pub struct GdiText {
    dc: HDC,
    dib: windows::Win32::Graphics::Gdi::HBITMAP,
    bits: *mut u8,
    cap_w: i32,
    cap_h: i32,
    stride: usize,
    family: Vec<u16>,
    fonts: HashMap<(u16, u16), FontEntry>,
    masks: HashMap<u64, Mask>,
}

impl GdiText {
    pub unsafe fn new(family: PCWSTR) -> Option<Self> {
        let dc = CreateCompatibleDC(None);
        if dc.0.is_null() {
            return None;
        }
        let mut wide: Vec<u16> = family.as_wide().to_vec();
        wide.push(0);
        let mut text = Self {
            dc,
            dib: Default::default(),
            bits: std::ptr::null_mut(),
            cap_w: 0,
            cap_h: 0,
            stride: 0,
            family: wide,
            fonts: HashMap::new(),
            masks: HashMap::new(),
        };
        if !text.ensure_capacity(512, 64) {
            text.release_dib();
            let _ = DeleteDC(dc);
            return None;
        }
        Some(text)
    }

    unsafe fn release_dib(&mut self) {
        if !self.dib.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(self.dib.0));
            self.dib = Default::default();
        }
    }

    unsafe fn ensure_capacity(&mut self, width: i32, height: i32) -> bool {
        if width <= self.cap_w && height <= self.cap_h {
            return true;
        }
        let cap_w = self.cap_w.max(width);
        let cap_h = self.cap_h.max(height);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = cap_w.max(1);
        bmi.bmiHeader.biHeight = -cap_h.max(1);
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(Some(self.dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(dib) => dib,
            Err(_) => return false,
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(dib.0));
            return false;
        }
        self.release_dib();
        let old = SelectObject(self.dc, HGDIOBJ(dib.0));
        if !old.0.is_null() {
            let _ = DeleteObject(old);
        }
        self.dib = dib;
        self.bits = bits as *mut u8;
        self.cap_w = cap_w;
        self.cap_h = cap_h;
        self.stride = cap_w as usize * 4;
        true
    }

    unsafe fn font(&mut self, size: f32, weight: u16) -> Option<&FontEntry> {
        let key = (size.round().max(1.0) as u16, weight);
        if !self.fonts.contains_key(&key) {
            let family = PCWSTR(self.family.as_ptr());
            let font = CreateFontW(
                -(key.0 as i32),
                0,
                0,
                0,
                key.1 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                family,
            );
            if font.0.is_null() {
                return None;
            }
            let previous = SelectObject(self.dc, HGDIOBJ(font.0));
            let mut metrics = TEXTMETRICW::default();
            let ascent = if GetTextMetricsW(self.dc, &mut metrics).as_bool() {
                metrics.tmAscent
            } else {
                (key.0 as f32 * 0.88) as i32
            };
            // GDI's tmAscent uses usWinAscent (1810/1000 for Chiron), which is
            // a clipping bound, not the typographic baseline. OUTLINETEXTMETRIC
            // carries hhea's ascent/descent/lineGap, the same values DWrite
            // used for its line metrics.
            let mut baseline_ratio = 0.88;
            let mut height_ratio = 1.2;
            let needed = GetOutlineTextMetricsW(self.dc, 0, None);
            if needed > 0 {
                let mut buffer = vec![0u64; (needed as usize + 7) / 8];
                let otm = buffer.as_mut_ptr() as *mut OUTLINETEXTMETRICW;
                if GetOutlineTextMetricsW(self.dc, needed, Some(otm)) > 0 {
                    let otm = &*otm;
                    // OUTLINETEXTMETRIC's ascent/descent/lineGap are already in
                    // logical device units at the selected height (Chiron's
                    // em square reports 1000 but is not the divisor here).
                    let px = key.0 as f32;
                    baseline_ratio = otm.otmAscent as f32 / px;
                    height_ratio =
                        (otm.otmAscent - otm.otmDescent + otm.otmLineGap as i32) as f32 / px;
                }
            }
            if !previous.0.is_null() {
                let _ = SelectObject(self.dc, previous);
            }
            self.fonts.insert(
                key,
                FontEntry {
                    font,
                    ascent: ascent.max(1),
                    baseline_ratio: baseline_ratio.max(0.1),
                    height_ratio: height_ratio.max(0.1),
                },
            );
        }
        self.fonts.get(&key)
    }

    /// Typographic metrics for this face: (baseline, line height) in pixels.
    /// These are GDI's OUTLINETEXTMETRIC values, not DirectWrite's.
    pub unsafe fn metrics(&mut self, size: f32, weight: u16) -> (f32, f32) {
        match self.font(size, weight) {
            Some(entry) => (
                entry.baseline_ratio * size,
                entry.height_ratio * size,
            ),
            None => (size * 0.88, size * 1.2),
        }
    }

    unsafe fn measure(&mut self, text: &str, width: i32, multiline: bool) -> (i32, i32) {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width.max(1),
            bottom: 0,
        };
        let mut flags = DT_CALCRECT | DT_NOPREFIX | DT_TOP;
        flags |= if multiline { DT_WORDBREAK } else { DT_SINGLELINE };
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        DrawTextW(self.dc, &mut wide, &mut rect, flags);
        (rect.right.max(0), rect.bottom.max(0))
    }

    /// Longest prefix that still fits, with a real U+2026 ellipsis.
    unsafe fn trim(&mut self, text: &str, width: i32, height: i32, multiline: bool) -> String {
        // GDI's line box for this family is much taller than the design's
        // 20-22px text rows, so a single-line string only has to fit the
        // width; the bitmap height already clips anything taller.
        let fits = |w: i32, h: i32| w <= width && (!multiline || h <= height);
        let (w, h) = self.measure(text, width, multiline);
        if fits(w, h) {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        let mut lo = 0usize;
        let mut hi = chars.len();
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let mut candidate: String = chars[..mid].iter().collect();
            candidate.push('\u{2026}');
            let (w, h) = self.measure(&candidate, width, multiline);
            if fits(w, h) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let mut out: String = chars[..lo].iter().collect();
        out.push('\u{2026}');
        out
    }

    /// Greedy character wrap at `width`, matching the layout's line height so
    /// multiline blocks keep the paragraph spacing the caller sized the box
    /// for. GDI's own line height for this family is nearly twice the typo
    /// metrics, which clipped second lines.
    unsafe fn wrap_lines(&mut self, text: &str, width: i32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut current = String::new();
        for ch in text.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut current));
                continue;
            }
            if current.is_empty() {
                current.push(ch);
                continue;
            }
            let mut candidate = current.clone();
            candidate.push(ch);
            if self.measure(&candidate, width, false).0 > width {
                lines.push(std::mem::take(&mut current));
                current.push(ch);
            } else {
                current = candidate;
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn mask(
        &mut self,
        target: &ID2D1RenderTarget,
        text: &str,
        style: Style,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        align: u8,
        ellipsis: bool,
        multiline: bool,
        color: [f32; 4],
    ) -> Option<(ID2D1Bitmap, f32, f32)> {
        if text.is_empty() {
            return None;
        }
        let key = mask_key(text, style, w, h, align, ellipsis, multiline, color);
        if let Some(mask) = self.masks.get(&key) {
            return Some((mask.bitmap.clone(), mask.w, mask.h));
        }
        let _ = (x, y);
        let mw = w.ceil().max(1.0) as i32;
        // Match the lab's box for single-line text: the mask is as tall as the
        // line box, not just the caller's row, so nothing clips.
        let mh = if multiline {
            h.ceil().max(1.0) as i32
        } else {
            (h.ceil().max(style.line_height * 1.6)).max(1.0) as i32
        };
        let mask_h = mh + PAD * 2;
        let ss = SUPERSAMPLE as i32;
        let dw = mw * ss;
        let dh = mask_h * ss;
        if !self.ensure_capacity(dw, dh) {
            return None;
        }

        // Lay out at 1x so wrapping and ellipsis widths match the live UI.
        let layout_font = self.font(style.size, style.weight).map(|entry| entry.font);
        let previous = layout_font.map(|font| SelectObject(self.dc, HGDIOBJ(font.0)));
        let fitted = if multiline {
            text.to_string()
        } else if ellipsis {
            self.trim(text, mw, mh, false)
        } else {
            text.to_string()
        };
        let mut lines = if fitted.is_empty() {
            Vec::new()
        } else if multiline {
            self.wrap_lines(&fitted, mw)
        } else {
            vec![fitted]
        };
        let line_height = style.line_height.max(1.0);
        let max_lines = ((mh as f32 / line_height).floor() as usize).max(1);
        if lines.len() > max_lines {
            lines.truncate(max_lines);
            if ellipsis {
                let mut last = lines.pop().unwrap_or_default();
                while !last.is_empty() {
                    let mut probe = last.clone();
                    probe.push('\u{2026}');
                    if self.measure(&probe, mw, false).0 <= mw {
                        last = probe;
                        break;
                    }
                    last.pop();
                }
                lines.push(last);
            }
        }
        if let Some(previous) = previous {
            let _ = SelectObject(self.dc, previous);
        }
        if lines.is_empty() {
            return None;
        }

        // Rasterise the glyphs. GDI's rasteriser synthesises grid-fitting even
        // for fonts without TrueType instructions (Chiron has none), so stems
        // land on whole pixels.
        for row in 0..dh {
            std::ptr::write_bytes(self.bits.add(row as usize * self.stride), 0, dw as usize * 4);
        }
        {
            let (ascent, draw_font) = match self.font(style.size * ss as f32, style.weight) {
                Some(entry) => (entry.ascent, entry.font),
                None => return None,
            };
            let previous = SelectObject(self.dc, HGDIOBJ(draw_font.0));
            let _ = SetBkMode(self.dc, TRANSPARENT);
            let _ = SetBkColor(self.dc, COLORREF(0));
            let _ = SetTextColor(self.dc, COLORREF(0x00ff_ffff));
            for (index, line) in lines.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                let baseline =
                    (PAD as f32 + (style.baseline + index as f32 * line_height)) * ss as f32;
                let mut area = RECT {
                    left: 0,
                    top: baseline.round() as i32 - ascent,
                    right: dw,
                    bottom: (PAD + mh) * ss,
                };
                let mut flags = DT_TOP | DT_NOPREFIX | DT_SINGLELINE;
                flags |= match align {
                    1 => DT_CENTER,
                    2 => DT_RIGHT,
                    _ => DRAW_TEXT_FORMAT(0),
                };
                let mut wide: Vec<u16> = line.encode_utf16().collect();
                DrawTextW(self.dc, &mut wide, &mut area, flags);
            }
            if !previous.0.is_null() {
                let _ = SelectObject(self.dc, previous);
            }
        }

        // Coverage is white-on-black; box-average the 5x5 blocks back to 1x,
        // then sharpen the 1x coverage with a 3x3 unsharp mask before tinting.
        let alpha = color[3].clamp(0.0, 1.0);
        let (cr, cg, cb) = (
            color[0].clamp(0.0, 1.0) * alpha,
            color[1].clamp(0.0, 1.0) * alpha,
            color[2].clamp(0.0, 1.0) * alpha,
        );
        let samples = (ss * ss) as f32;
        let mut coverage = vec![0u8; (mw * mask_h) as usize];
        for row in 0..mask_h as usize {
            for column in 0..mw as usize {
                let mut sum = 0u32;
                for sy in 0..ss as usize {
                    let source = self
                        .bits
                        .add((row * ss as usize + sy) * self.stride);
                    for sx in 0..ss as usize {
                        sum += *source.add((column * ss as usize + sx) * 4 + 2) as u32;
                    }
                }
                coverage[row * mw as usize + column] =
                    (sum as f32 / samples + 0.5) as u8;
            }
        }
        let mut sharpened = coverage.clone();
        for row in 0..mask_h as usize {
            for column in 0..mw as usize {
                let center = coverage[row * mw as usize + column] as f32;
                let mut blur = 0.0f32;
                for ky in 0..3usize {
                    let sy = (row as i32 + ky as i32 - 1).clamp(0, mask_h - 1) as usize;
                    for kx in 0..3usize {
                        let sx = (column as i32 + kx as i32 - 1).clamp(0, mw - 1) as usize;
                        let weight = match (kx, ky) {
                            (1, 1) => 4.0,
                            (0, 1) | (2, 1) | (1, 0) | (1, 2) => 2.0,
                            _ => 1.0,
                        };
                        blur += coverage[sy * mw as usize + sx] as f32 * weight;
                    }
                }
                blur /= 16.0;
                let value = center + SHARPEN * (center - blur);
                sharpened[row * mw as usize + column] =
                    value.clamp(0.0, 255.0) as u8;
            }
        }
        let mut pixels = vec![0u8; (mw * mask_h * 4) as usize];
        for row in 0..mask_h as usize {
            for column in 0..mw as usize {
                let coverage = sharpened[row * mw as usize + column] as f32 / 255.0;
                if coverage <= 0.0 {
                    continue;
                }
                let index = (row * mw as usize + column) * 4;
                pixels[index] = (cb * coverage * 255.0 + 0.5) as u8;
                pixels[index + 1] = (cg * coverage * 255.0 + 0.5) as u8;
                pixels[index + 2] = (cr * coverage * 255.0 + 0.5) as u8;
                pixels[index + 3] = (alpha * coverage * 255.0 + 0.5) as u8;
            }
        }
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        let bitmap = target
            .CreateBitmap(
                D2D_SIZE_U {
                    width: mw as u32,
                    height: mask_h as u32,
                },
                Some(pixels.as_ptr() as *const c_void),
                (mw * 4) as u32,
                &props,
            )
            .ok()?;
        if self.masks.len() > MAX_MASKS {
            self.masks.clear();
        }
        self.masks.insert(
            key,
            Mask {
                bitmap: bitmap.clone(),
                w: mw as f32,
                h: mask_h as f32,
            },
        );
        Some((bitmap, mw as f32, mask_h as f32))
    }

    /// Draw `text` inside the box at (`x`, `y`, `w`, `h`). `align` is
    /// 0 = left, 1 = centre, 2 = right. `style.baseline` is the distance from
    /// the box top to the first text baseline.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn draw(
        &mut self,
        target: &ID2D1RenderTarget,
        text: &str,
        style: Style,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        align: u8,
        ellipsis: bool,
        multiline: bool,
        color: [f32; 4],
    ) {
        let left = x.round();
        let top = y.round();
        let Some((bitmap, mask_w, mask_h)) = self.mask(
            target, text, style, x, y, w, h, align, ellipsis, multiline, color,
        ) else {
            return;
        };
        let dest = D2D_RECT_F {
            left,
            top: top - PAD as f32,
            right: left + mask_w,
            bottom: top - PAD as f32 + mask_h,
        };
        target.DrawBitmap(
            &bitmap,
            Some(&dest),
            1.0,
            D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            None,
        );
    }
}

fn mask_key(
    text: &str,
    style: Style,
    w: f32,
    h: f32,
    align: u8,
    ellipsis: bool,
    multiline: bool,
    color: [f32; 4],
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    style.size.to_bits().hash(&mut hasher);
    style.weight.hash(&mut hasher);
    ((w * 10.0) as u32).hash(&mut hasher);
    ((h * 10.0) as u32).hash(&mut hasher);
    align.hash(&mut hasher);
    ellipsis.hash(&mut hasher);
    multiline.hash(&mut hasher);
    for channel in color {
        (channel.clamp(0.0, 1.0) * 255.0).round().to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

impl Drop for GdiText {
    fn drop(&mut self) {
        unsafe {
            for (_, entry) in self.fonts.drain() {
                let _ = DeleteObject(HGDIOBJ(entry.font.0));
            }
            self.release_dib();
            let _ = DeleteDC(self.dc);
        }
    }
}
