//! f207: 弹幕条带 — rasterise each comment once, blit it every frame.
//!
//! # Why
//!
//! f200 measured the comment layer as a Slint `MinimalSoftwareWindow` that is
//! re-rendered in full on every tick: 4.7-9.9 ms at 1280x720, **10-32 ms at
//! 1920x1080**. The text is the cost - the same glyphs are re-shaped and
//! re-rasterised 40-70 times a second only because their x changed - so the
//! layer cannot be presented at 60 Hz on a 1080p picture however the tick is
//! tuned (the numbers are in [`DanmakuStats`](super::danmaku::DanmakuStats)).
//!
//! Here the shaped text leaves the per-frame path entirely:
//!
//! * each distinct comment is rasterised **once** into a small 8-bit coverage
//!   mask, keyed by `(dmid, pixel size, bold)`, and kept in a texture atlas
//!   with an LRU page budget (64 MB by default);
//! * a frame is one clear plus one draw call per atlas page over quads whose x
//!   is the same number the motion lane already publishes - no shaping, no
//!   layout, no CPU copy of the frame. f210: the clear and the draws go to an
//!   offscreen target, and the flip-model backbuffer is written by a single
//!   `CopyResource` just before the present.
//!
//! The colour is a per-vertex constant, so 弹幕透明度 and a colour change cost
//! nothing in the cache: the mask carries coverage only.
//!
//! # Rasterisation: the bundled faces, GDI-interop coverage
//!
//! The plan named `IDWriteBitmapRenderTarget`. `windows` 0.62 does not export
//! `IDWriteGdiInterop`/`IDWriteBitmapRenderTarget` at all (checked in the crate
//! source: neither name exists), so using them would mean hand-declaring two
//! COM interfaces and their vtables. That object rasterises through GDI anyway
//! - the mask *is* a `CreateDIBSection` bitmap that GDI fills - so this module
//! takes the same road without the hand-written COM:
//! `AddFontMemResourceEx` installs the bundled `O11A1Round` faces from memory
//! (the two files `ui/app.slint` imports, so the same glyphs at the same em
//! size) and `TextOutW` draws white ink on a zeroed 32-bit DIB. The coverage
//! is the per-pixel max of the three colour channels, which is what
//! `IDWriteBitmapRenderTarget` returns for the same grayscale run.
//!
//! `ANTIALIASED_QUALITY` on purpose: ClearType would split the coverage across
//! the colour channels of a bitmap that has no opaque background.
//!
//! # What stays the same
//!
//! Positions, speed, lane assignment, density and the 960x540 -> picture-rect
//! mapping are the engine's and untouched: this module only replaces *how* an
//! item that already has (x, y, size, colour) reaches the swapchain. A mask is
//! placed at `(x, y) * (height / 540)` - exactly the transform
//! `DanmakuOverlay` applies - so lane height, the clip to the picture rect and
//! the right-to-left travel keep their meaning to the pixel.

use std::collections::HashMap;
use std::ffi::c_void;
use std::time::Instant;

use windows::core::{BOOL, PCSTR};
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_VERTEX_BUFFER,
    D3D11_BLEND_DESC, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA,
    D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER, D3D11_CULL_NONE,
    D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_INPUT_ELEMENT_DESC,
    D3D11_INPUT_PER_VERTEX_DATA, D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC,
    D3D11_BOX, D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11BlendState,
    ID3D11Buffer, ID3D11ClassLinkage, ID3D11DepthStencilView, ID3D11Device,
    ID3D11DeviceContext, ID3D11InputLayout, ID3D11PixelShader, ID3D11RasterizerState,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R32G32_FLOAT,
    DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;

use windows_sys::Win32::Graphics::Gdi::{
    AddFontMemResourceEx, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    GetTextExtentPoint32W, GetTextFaceW, GetTextMetricsW, SelectObject, SetBkMode, SetTextColor,
    TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DIB_RGB_COLORS, HBITMAP, HDC, HFONT, RGBQUAD, TEXTMETRICW, TRANSPARENT, OUT_TT_PRECIS,
};

use super::warp;

/// The engine's logical space (`player.rs::VIDEO_W/H`). A mask is placed with
/// the same `height / 540` the Slint overlay uses.
const LOGICAL_H: f32 = 540.0;

/// Transparent margin around a mask. Anti-aliased glyph edges are coverage on
/// the first and last column/row of the cell; a bitmap that starts exactly at
/// the ink loses them.
const PAD: i32 = 1;

/// Atlas page: 4096x2048 `R8_UNORM` = 8 MiB per page. Room for ~38 rows of
/// 52 px text per page at 1080p.
const PAGE_W: u32 = 4096;
const PAGE_H: u32 = 2048;
const PAGE_BYTES: u64 = PAGE_W as u64 * PAGE_H as u64;

/// Default cache budget (`BILI_DANMAKU_STRIPS_MB` overrides, A/B only).
const DEFAULT_CAP_MB: u64 = 64;

/// A mask never needs to be wider than a page.
const MAX_MASK_W: u32 = PAGE_W - 2 * PAD as u32;

/// One vertex: layer pixels, atlas texels, straight (non-premultiplied) colour.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Vertex {
    x: f32,
    y: f32,
    u: f32,
    v: f32,
    color: [f32; 4],
}

struct Page {
    texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
    /// Next free x on the current shelf, and that shelf's top and height.
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    /// LRU stamp of the newest entry on this page.
    used: u64,
}

#[derive(Clone, Copy)]
struct Located {
    page: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    /// Vertical correction for this mask's em size (see [`Mask::dy`]).
    dy: i32,
}

type Key = (slint::SharedString, u32, bool);

/// A rasterised comment: coverage only, tightly packed, plus the vertical
/// correction between GDI's cell and the line box Slint uses.
struct Mask {
    width: u32,
    height: u32,
    /// Pixels to move the mask up so its ink lands where `DanmakuOverlay`'s
    /// `Text` would put it (see [`face_metrics`] and [`Raster::font`]).
    dy: i32,
}

/// The vertical metrics this rasteriser needs, straight out of the embedded
/// TTF: `unitsPerEm`, the `OS/2` typographic ascender and the `hhea` ascender,
/// all in font units.
///
/// Slint lays a `Text` item out with the font's *typographic* ascent (0.94 em
/// for this face); GDI positions a run from `tmAscent`, which follows
/// `usWinAscent`/`hhea` (1.16 em). Placing the raw GDI cell at the item's y
/// therefore drops every comment by `(1.16 - 0.94) em` - measured at 9 px for a
/// 40 px em at 1080p and 5-6 px for 26.7 px at 720p against the f195 capture -
/// so the difference is computed here and subtracted at draw time, scaled with
/// the em instead of hard-coded per size.
fn face_metrics(bytes: &[u8]) -> (u16, i16, i16) {
    fn u16_at(data: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([data[at], data[at + 1]])
    }
    fn u32_at(data: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
    }
    let count = u16_at(bytes, 4) as usize;
    let (mut upem, mut hhea, mut typo) = (1000u16, 800i16, 0i16);
    for index in 0..count {
        let record = 12 + index * 16;
        if record + 16 > bytes.len() {
            break;
        }
        let tag = &bytes[record..record + 4];
        let offset = u32_at(bytes, record + 8) as usize;
        match tag {
            b"head" if offset + 20 <= bytes.len() => upem = u16_at(bytes, offset + 18),
            b"hhea" if offset + 10 <= bytes.len() => hhea = u16_at(bytes, offset + 4) as i16,
            // OS/2 version >= 1 puts sTypoAscender at 68; version 0's table is
            // too short, so the `hhea` value stays the fallback.
            b"OS/2" if offset + 70 <= bytes.len() => typo = u16_at(bytes, offset + 68) as i16,
            _ => {}
        }
    }
    (upem.max(1), if typo > 0 { typo } else { hhea }, hhea)
}

/// The mask rasteriser: one memory DC, one HFONT per (size, weight), one DIB
/// at a time. UI thread only, like the layer that owns it.
struct Raster {
    dc: HDC,
    /// HFONT plus the vertical correction for that size (`dy`, see [`Mask`]).
    fonts: HashMap<(u32, bool), (HFONT, i32)>,
    /// The font currently selected into `dc`, so it is only re-selected when the
    /// size or the weight changes.
    selected: Option<(u32, bool)>,
    /// One DIB section, kept selected for the life of the rasteriser.
    ///
    /// Creating a `CreateDIBSection`, selecting it, drawing, de-selecting and
    /// deleting it *per comment* cost ~4 ms per mask here (measured: 5 warm
    /// renders in 21 ms) because GDI re-realises the font every time the DC's
    /// bitmap changes. One long-lived DIB brings a mask down to tens of
    /// microseconds, which matters: 30 new comments arriving in one frame is
    /// normal, and a 120 ms hitch in the middle of a present is what the
    /// compositor does not survive.
    dib: Option<Dib>,
    scratch: Vec<u8>,
    /// Typographic ascender in em units (`sTypoAscender / unitsPerEm`): the
    /// baseline Slint draws a `Text` item at.
    typo_ascender_per_em: f32,
    /// Font installation result, logged once.
    face: String,
}

/// The long-lived bitmap the rasteriser draws into: top-down 32-bit BGRA.
struct Dib {
    bitmap: HBITMAP,
    bits: *mut c_void,
    width: u32,
    height: u32,
}

impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: the bitmap is this struct's own; the bits pointer dies with it.
        unsafe {
            let _ = DeleteObject(self.bitmap);
        }
    }
}

impl Raster {
    fn new() -> Result<Self, String> {
        let face = installed_faces()?;
        // SAFETY: `CreateCompatibleDC(NULL)` is the documented way to get a
        // memory DC for a DIB; the DC is released in `Drop`.
        let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        if dc.is_null() {
            return Err("CreateCompatibleDC failed".into());
        }
        let (upem, ascender, _) = face_metrics(REGULAR);
        Ok(Self {
            dc,
            fonts: HashMap::new(),
            selected: None,
            dib: None,
            scratch: Vec::new(),
            typo_ascender_per_em: ascender as f32 / upem as f32,
            face,
        })
    }

    fn font(&mut self, size_px: u32, bold: bool) -> Result<(HFONT, i32), String> {
        if let Some(font) = self.fonts.get(&(size_px, bold)) {
            return Ok(*font);
        }
        let face: Vec<u16> = FACE.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the face name is NUL-terminated and outlives the call.
        let font = unsafe {
            CreateFontW(
                // Negative height = em height, the number Slint's `font-size`
                // names, so a 52 px item is rasterised at 52 px.
                -(size_px as i32),
                0,
                0,
                0,
                if bold { 700 } else { 400 },
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_TT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                // ANTIALIASED_QUALITY: grayscale AA, see the module docs.
                4,
                0,
                face.as_ptr(),
            )
        };
        if font.is_null() {
            return Err(format!("CreateFontW size={size_px} bold={bold} failed"));
        }
        // The font mapper silently substitutes another face when the family is
        // not there, and a comment layer in 微软雅黑 is exactly what this module
        // must not do: check what was actually selected.
        let mut name = [0u16; 64];
        let (resolved, ascent) = unsafe {
            SelectObject(self.dc, font);
            let length = GetTextFaceW(self.dc, name.len() as i32, name.as_mut_ptr());
            let length = (length.max(1) as usize - 1).min(name.len());
            let mut metrics: TEXTMETRICW = std::mem::zeroed();
            let ascent = if GetTextMetricsW(self.dc, &mut metrics) != 0 {
                metrics.tmAscent as f32
            } else {
                self.typo_ascender_per_em * size_px as f32
            };
            (String::from_utf16_lossy(&name[..length]), ascent)
        };
        if !resolved.trim().is_empty() && !resolved.trim().eq_ignore_ascii_case(FACE) {
            unsafe {
                let _ = DeleteObject(font);
            }
            return Err(format!(
                "font mapper resolved \"{resolved}\" instead of \"{FACE}\" (size={size_px} \
                 bold={bold})"
            ));
        }
        let dy = (ascent - self.typo_ascender_per_em * size_px as f32).round() as i32;
        self.fonts.insert((size_px, bold), (font, dy));
        Ok((font, dy))
    }

    /// Draw `text` into a zeroed 32-bit DIB and keep its coverage.
    /// Ensure a DIB at least `width`x`height`, selected into the memory DC.
    fn ensure_dib(&mut self, width: u32, height: u32) -> Result<(), String> {
        if let Some(dib) = &self.dib {
            if dib.width >= width && dib.height >= height {
                return Ok(());
            }
        }
        // Grow geometrically, never shrink. Every new DIB selection makes GDI
        // re-realise the font (~14 ms here), and an exactly-sized DIB grows on
        // almost every comment: 31 masks cost 235 ms that way, which stalls a
        // present for a quarter of a second. Doubling turns that into ~3
        // realisations for a whole session.
        let width = width.max(self.dib.as_ref().map_or(0, |dib| dib.width)).max(512);
        let width = width.next_power_of_two().min(MAX_MASK_W).max(width.min(MAX_MASK_W));
        let height = height.max(self.dib.as_ref().map_or(0, |dib| dib.height)).max(64);
        let height = height.next_power_of_two().min(PAGE_H).max(height.min(PAGE_H));
        if let Some(old) = self.dib.take() {
            // SAFETY: deselect before the bitmap dies, or the DC keeps a
            // reference to freed pixels.
            unsafe {
                SelectObject(self.dc, std::ptr::null_mut());
            }
            drop(old);
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative = top-down rows, the order the atlas wants.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }],
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: `info` describes the bitmap; the returned bits stay valid
        // until the bitmap is deleted (`Dib::drop`), which is after the DC has
        // been told to let go of it.
        let bitmap = unsafe {
            CreateDIBSection(
                self.dc,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            )
        };
        if bitmap.is_null() || bits.is_null() {
            return Err("CreateDIBSection failed".into());
        }
        unsafe {
            SelectObject(self.dc, bitmap);
        }
        self.dib = Some(Dib {
            bitmap,
            bits,
            width,
            height,
        });
        Ok(())
    }

    fn render(&mut self, text: &str, size_px: u32, bold: bool) -> Result<Mask, String> {
        let (font, dy) = self.font(size_px, bold)?;
        if self.selected != Some((size_px, bold)) {
            // SAFETY: the HFONT is owned by `self.fonts` and outlives the DC.
            unsafe {
                SelectObject(self.dc, font);
            }
            self.selected = Some((size_px, bold));
        }
        let wide: Vec<u16> = text.encode_utf16().collect();
        if wide.is_empty() {
            return Err("empty text".into());
        }
        let mut extent = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
        let trace = std::env::var("BILI_STRIPS_TRACE").is_ok();
        let t0 = std::time::Instant::now();
        // SAFETY: `dc` is this struct's memory DC with the right font and DIB
        // selected; `wide` outlives the call.
        unsafe {
            if GetTextExtentPoint32W(self.dc, wide.as_ptr(), wide.len() as i32, &mut extent) == 0 {
                return Err("GetTextExtentPoint32W failed".into());
            }
        }
        // A comment wider than a page cannot be fully on screen anyway; draw
        // the head of it rather than dropping it (`TextOutW` clips at the DIB).
        let width = (extent.cx.max(1) as u32 + 2 * PAD as u32).min(MAX_MASK_W);
        let height = (extent.cy.max(1) as u32 + 2 * PAD as u32).min(PAGE_H);
        let t_extent = t0.elapsed();
        self.ensure_dib(width, height)?;
        let t_dib = t0.elapsed();
        let dib = self.dib.as_ref().ok_or("no DIB")?;
        let pitch = dib.width as usize * 4;
        let bytes = pitch * height as usize;
        // SAFETY: the DIB is at least `width`x`height` and is selected into the
        // DC; `bits` points at `pitch * dib.height` bytes.
        unsafe {
            let pixels = std::slice::from_raw_parts_mut(dib.bits as *mut u8, pitch * dib.height as usize);
            // Only the rows this mask uses have to be zeroed.
            pixels[..bytes].fill(0);
            SetBkMode(self.dc, TRANSPARENT as i32);
            // White ink: the colour channels then carry the coverage.
            SetTextColor(self.dc, 0x00ff_ffff);
            TextOutW(
                self.dc,
                PAD,
                PAD,
                wide.as_ptr(),
                wide.len().min(i32::MAX as usize) as i32,
            );
            // Coverage = max(r, g, b): grayscale AA makes the three equal, and
            // the max also survives a ClearType fallback without dropping its
            // side coverage. `scratch` is reused across calls.
            self.scratch.clear();
            self.scratch.reserve(width as usize * height as usize);
            let source = std::slice::from_raw_parts(dib.bits as *const u8, bytes);
            for row in source.chunks_exact(pitch) {
                for pixel in row[..width as usize * 4].chunks_exact(4) {
                    self.scratch.push(pixel[0].max(pixel[1]).max(pixel[2]));
                }
            }
        }
        if trace {
            eprintln!(
                "danmaku strips raster size={width}x{height} extent={:?} dib={:?} total={:?}",
                t_extent,
                t_dib - t_extent,
                t0.elapsed()
            );
        }
        Ok(Mask { width, height, dy })
    }

    fn pixels(&self) -> &[u8] {
        &self.scratch
    }
}

impl Drop for Raster {
    fn drop(&mut self) {
        unsafe {
            // The DC must let go of the bitmap before either dies.
            let _ = SelectObject(self.dc, std::ptr::null_mut());
            self.dib = None;
            for (_, (font, _)) in self.fonts.drain() {
                let _ = DeleteObject(font);
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Install the two bundled faces once per process and report what the font
/// mapper saw.
fn installed_faces() -> Result<String, String> {
    static FONTS: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut installed = 0u32;
            for bytes in [REGULAR, BOLD] {
                let mut count = 0u32;
                // SAFETY: the slices are 'static and never freed; the API only
                // reads them and keeps them mapped for the process lifetime.
                let handle = unsafe {
                    AddFontMemResourceEx(
                        bytes.as_ptr() as *const c_void,
                        bytes.len() as u32,
                        std::ptr::null(),
                        &mut count,
                    )
                };
                if handle.is_null() || count == 0 {
                    return Err(format!(
                        "AddFontMemResourceEx rejected a {} byte face",
                        bytes.len()
                    ));
                }
                installed += count;
            }
            Ok(format!("faces={installed}"))
        })
        .clone()
}

/// The bundled faces: the same two files `ui/app.slint` imports. Embedded, not
/// read from disk, because the shipped package has no `assets/fonts` (the Slint
/// build compiles them into the binary) and a comment layer quietly falling
/// back to 微软雅黑 would not look like this app.
static REGULAR: &[u8] = include_bytes!("../../assets/fonts/O11A1Round-Regular.ttf");
static BOLD: &[u8] = include_bytes!("../../assets/fonts/O11A1Round-Bold.ttf");
const FACE: &str = "O11A1 Round";

/// Whether the strips path is armed.
///
/// **On by default since f210.** f207 shipped it opt-in because in about half
/// of the runs the process died 10-40 s in with an access violation inside
/// `nvwgf2umx.dll` (0xc0000005, faulting offset `+0xeabfeb` every time).
///
/// The cause was not the flip-model backbuffer: it was the vertex upload.
/// `UpdateSubresource` on a *buffer* ignores the box and copies the resource's
/// whole `ByteWidth` from `pSrcData`, and this path passed the address of a
/// Vec sized by the *current* frame (`verts * 32` bytes) to a buffer allocated
/// for 8192 vertices - so the user-mode driver read up to ~240 KB past the end
/// of a heap block on every frame. Usually that lands in mapped memory (the
/// run survives), occasionally it does not (`0xc0000005` inside the driver).
/// See `Compositor::upload`: the staging image is now exactly the buffer's
/// size. f210's 3 x 60 s soak at 1080p with this path is 0 crashes.
///
/// Two smaller changes came with it and stay: the frame is rendered into an
/// offscreen `BIND_RENDER_TARGET` texture (one view, created with the size)
/// and reaches the flip-model backbuffer by a single `CopyResource` just before
/// the present, which removes both the per-frame backbuffer RTV and any clear
/// of a surface DWM owns.
///
/// `BILI_DANMAKU_STRIPS=0` puts the f196 Slint component back for A/B (it is
/// also the fallback when anything here fails to initialise).
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("BILI_DANMAKU_STRIPS") {
        Ok(value) => !matches!(value.trim(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    })
}

/// The per-frame cost ring, µs.
pub(crate) struct Samples {
    values: Vec<u32>,
    at: usize,
}

impl Samples {
    fn new(capacity: usize) -> Self {
        Self {
            values: vec![0; capacity],
            at: 0,
        }
    }

    fn push(&mut self, value: u32) {
        self.values[self.at] = value;
        self.at = (self.at + 1) % self.values.len();
    }

    /// `(p50, p95)`, ignoring not-yet-filled slots. Sorts a copy; read at a few
    /// Hz, never per frame.
    pub(crate) fn percentiles(&self) -> (u32, u32) {
        let mut values: Vec<u32> = self.values.iter().copied().filter(|v| *v > 0).collect();
        if values.is_empty() {
            return (0, 0);
        }
        values.sort_unstable();
        let pick = |percent: f32| {
            let index = ((values.len() - 1) as f32 * percent / 100.0).round() as usize;
            values[index.min(values.len() - 1)]
        };
        (pick(50.0), pick(95.0))
    }
}

/// What one composed frame reports back to the layer.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Frame {
    /// Compose cost in µs (build the quads + draw + present).
    pub(crate) us: u32,
    /// Whether the compositor took the frame (`Present` returned `S_OK`).
    pub(crate) presented: bool,
}

/// What the QA line reads.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CacheStats {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) strips: u32,
    pub(crate) bytes: u64,
    pub(crate) quads: u32,
}

/// The draw-and-blit half of the danmaku layer.
pub(crate) struct Compositor {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    input: ID3D11InputLayout,
    blend: ID3D11BlendState,
    raster_state: ID3D11RasterizerState,
    sampler: ID3D11SamplerState,
    /// `float4(2/w, 2/h, 0, 0)`: the pixel -> clip transform in one constant.
    params: ID3D11Buffer,
    vertices: ID3D11Buffer,
    /// Vertices the buffer holds; fixed for the life of the compositor.
    capacity: usize,
    /// The exact byte image `UpdateSubresource` reads for `vertices`.
    ///
    /// A buffer upload is all-or-nothing: `UpdateSubresource` ignores the box
    /// and copies the buffer's whole `ByteWidth` from `pSrcData`, so a staging
    /// image shorter than the buffer is an out-of-bounds read *inside the
    /// driver*. f207 handed it `verts * 32` bytes of a Vec that was only that
    /// large while the buffer was 8192 vertices, i.e. the NVIDIA user-mode
    /// driver read up to 240 KB past the end of a heap block on every frame -
    /// usually into mapped memory, occasionally into a page that is not, which
    /// is the `nvwgf2umx.dll +0xeabfeb` 0xc0000005 that killed about half of
    /// the f207 runs 10-40 s in. This image is always exactly the buffer's
    /// size, so the read is bounded by construction (262 KB per frame is
    /// ~15 MB/s at 60 Hz - free).
    upload: Vec<u8>,
    /// Whether a frame ever needed more quads than the buffer holds.
    truncated: bool,
    pages: Vec<Page>,
    map: HashMap<Key, Located>,
    stamp: u64,
    cap: u64,
    bytes: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
    quads: u32,
    raster: Raster,
    /// Quads of this frame, page-tagged, then flattened into `flat`.
    scratch: Vec<(u32, [Vertex; 6])>,
    flat: Vec<Vertex>,
    /// The positions drawn last frame, for the "nothing moved" early out.
    last: Vec<(i32, i32)>,
    /// Force one paint (re-attach, resize) even if nothing moved.
    dirty: bool,
    target: (u32, u32),
    samples: Samples,
    logged: u64,
    /// f210: the offscreen target every frame is drawn into, and its view. The
    /// flip-model backbuffer only ever receives a `CopyResource` of this - see
    /// [`Self::draw`] and [`enabled`].
    target_tex: Option<ID3D11Texture2D>,
    target_rtv: Option<ID3D11RenderTargetView>,
    target_size: (u32, u32),
}

impl Compositor {
    pub(crate) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Self, String> {
        let (vs_code, ps_code) = shader()?;
        let raster = Raster::new()?;
        let cap_mb = std::env::var("BILI_DANMAKU_STRIPS_MB")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_CAP_MB)
            .clamp(8, 512);
        // SAFETY: every object below belongs to `device`; the bytecode is a
        // process-wide compile of the constant source in this file.
        unsafe {
            let mut vs: Option<ID3D11VertexShader> = None;
            device
                .CreateVertexShader(&vs_code, None::<&ID3D11ClassLinkage>, Some(&mut vs))
                .map_err(|error| format!("CreateVertexShader hr={error}"))?;
            let mut ps: Option<ID3D11PixelShader> = None;
            device
                .CreatePixelShader(&ps_code, None::<&ID3D11ClassLinkage>, Some(&mut ps))
                .map_err(|error| format!("CreatePixelShader hr={error}"))?;
            let (Some(vs), Some(ps)) = (vs, ps) else {
                return Err("shader creation returned no object".into());
            };
            let elements = [
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: PCSTR(b"POSITION\0".as_ptr()),
                    SemanticIndex: 0,
                    Format: DXGI_FORMAT_R32G32_FLOAT,
                    InputSlot: 0,
                    AlignedByteOffset: 0,
                    InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                    InstanceDataStepRate: 0,
                },
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: PCSTR(b"TEXCOORD\0".as_ptr()),
                    SemanticIndex: 0,
                    Format: DXGI_FORMAT_R32G32_FLOAT,
                    InputSlot: 0,
                    AlignedByteOffset: 8,
                    InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                    InstanceDataStepRate: 0,
                },
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: PCSTR(b"COLOR\0".as_ptr()),
                    SemanticIndex: 0,
                    Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                    InputSlot: 0,
                    AlignedByteOffset: 16,
                    InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                    InstanceDataStepRate: 0,
                },
            ];
            let mut input: Option<ID3D11InputLayout> = None;
            device
                .CreateInputLayout(&elements, &vs_code, Some(&mut input))
                .map_err(|error| format!("CreateInputLayout hr={error}"))?;
            // Premultiplied source over the cleared target: the pixel shader
            // already multiplied rgb by coverage*alpha.
            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: BOOL(1),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend: Option<ID3D11BlendState> = None;
            device
                .CreateBlendState(&blend_desc, Some(&mut blend))
                .map_err(|error| format!("CreateBlendState hr={error}"))?;
            let raster_desc = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                // Both windings: a quad is emitted in screen order, and the
                // cheap state is "draw it".
                CullMode: D3D11_CULL_NONE,
                FrontCounterClockwise: BOOL(0),
                DepthBias: 0,
                DepthBiasClamp: 0.0,
                SlopeScaledDepthBias: 0.0,
                DepthClipEnable: BOOL(1),
                ScissorEnable: BOOL(0),
                MultisampleEnable: BOOL(0),
                AntialiasedLineEnable: BOOL(0),
            };
            let mut rasterizer: Option<ID3D11RasterizerState> = None;
            device
                .CreateRasterizerState(&raster_desc, Some(&mut rasterizer))
                .map_err(|error| format!("CreateRasterizerState hr={error}"))?;
            let sampler_desc = D3D11_SAMPLER_DESC {
                // 1:1 texel-to-pixel with integer quad positions: point
                // sampling *is* the exact blit, linear would blur the mask.
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                BorderColor: [0.0; 4],
                MinLOD: 0.0,
                MaxLOD: 0.0,
            };
            let mut sampler: Option<ID3D11SamplerState> = None;
            device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(|error| format!("CreateSamplerState hr={error}"))?;
            let params_desc = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let initial = [0f32; 4];
            let source = D3D11_SUBRESOURCE_DATA {
                pSysMem: initial.as_ptr() as *const c_void,
                SysMemPitch: 0,
                SysMemSlicePitch: 0,
            };
            let mut params: Option<ID3D11Buffer> = None;
            device
                .CreateBuffer(&params_desc, Some(&source), Some(&mut params))
                .map_err(|error| format!("CreateBuffer params hr={error}"))?;
            let Some(input) = input else {
                return Err("no input layout".into());
            };
            let Some(blend) = blend else {
                return Err("no blend state".into());
            };
            let Some(raster_state) = rasterizer else {
                return Err("no rasterizer state".into());
            };
            let Some(sampler) = sampler else {
                return Err("no sampler state".into());
            };
            let Some(params) = params else {
                return Err("no params buffer".into());
            };
            let vertices = Self::vertex_buffer(device, 8192)?;
            crate::debug_log(&format!(
                "danmaku strips gpu ready stride={} cap_mb={cap_mb} fonts={}",
                std::mem::size_of::<Vertex>(),
                raster.face
            ));
            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vs,
                ps,
                input,
                blend,
                raster_state,
                sampler,
                params,
                vertices,
                capacity: 8192,
                upload: Vec::new(),
                truncated: false,
                pages: Vec::new(),
                map: HashMap::new(),
                stamp: 0,
                cap: cap_mb * 1024 * 1024,
                bytes: 0,
                hits: 0,
                misses: 0,
                evictions: 0,
                quads: 0,
                raster,
                scratch: Vec::new(),
                flat: Vec::new(),
                last: Vec::new(),
                dirty: true,
                target: (0, 0),
                samples: Samples::new(256),
                logged: 0,
                target_tex: None,
                target_rtv: None,
                target_size: (0, 0),
            })
        }
    }

    fn vertex_buffer(device: &ID3D11Device, verts: usize) -> Result<ID3D11Buffer, String> {
        let description = D3D11_BUFFER_DESC {
            ByteWidth: (verts * std::mem::size_of::<Vertex>()) as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut buffer: Option<ID3D11Buffer> = None;
        // SAFETY: a plain vertex buffer with no initial data.
        unsafe {
            device
                .CreateBuffer(&description, None, Some(&mut buffer))
                .map_err(|error| format!("CreateBuffer vertices hr={error}"))?;
        }
        buffer.ok_or_else(|| "no vertex buffer".to_string())
    }

    pub(crate) fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits,
            misses: self.misses,
            strips: self.map.len() as u32,
            bytes: self.bytes,
            quads: self.quads,
        }
    }

    pub(crate) fn percentiles(&self) -> (u32, u32) {
        self.samples.percentiles()
    }

    /// Ask for one more frame even with nothing moving (弹幕 re-attached, a
    /// resize the model has not caught up with).
    pub(crate) fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Draw `rows` into `swap`'s backbuffer.
    ///
    /// Returns the compose time in µs, or `None` when nothing has moved since
    /// the previous frame. `target` is the layer's size in physical pixels; the
    /// caller presents ([`Self::present`]) once the backbuffer reference is
    /// gone.
    pub(crate) fn compose(
        &mut self,
        rows: &[crate::DanmakuItem],
        target: (u32, u32),
        layer: &super::gpu::Layer,
    ) -> Result<Option<Frame>, String> {
        if target.0 < 2 || target.1 < 2 {
            return Ok(None);
        }
        let scale = target.1 as f32 / LOGICAL_H;
        let moved = self.dirty
            || target != self.target
            || rows.len() != self.last.len()
            || rows.iter().zip(self.last.iter()).any(|(row, last)| {
                last.0 != (row.x * scale).round() as i32
                    || last.1 != (row.y * scale).round() as i32
            });
        if !moved {
            return Ok(None);
        }
        // No frame-latency wait in front of the present. f196 measured the pair
        // this would need - wait, then `Present(DO_NOT_WAIT)` - and it stops a
        // composition layer from presenting at all ("the wait consumed the
        // signal and the following Present had nothing to hand over"); f207
        // reproduced it exactly: with the wait in place `dm_s` fell to ~0/s.
        // The layer keeps the f196 present order and a dropped present is
        // retried on the next tick (`DM_SKIPS`).
        let started = Instant::now();
        self.last.clear();
        self.last.reserve(rows.len());
        self.scratch.clear();
        for row in rows {
            let x = (row.x * scale).round() as i32;
            let y = (row.y * scale).round() as i32;
            self.last.push((x, y));
            let size = (row.size * scale).round().clamp(8.0, 512.0) as u32;
            let Some(located) = self.strip(&row.dmid, &row.text, size, row.bold) else {
                continue;
            };
            if located.w == 0 || located.h == 0 {
                continue;
            }
            let color = row.color;
            let shade = [
                color.red() as f32 / 255.0,
                color.green() as f32 / 255.0,
                color.blue() as f32 / 255.0,
                color.alpha() as f32 / 255.0,
            ];
            // The mask carries `PAD` of transparent margin and GDI's cell sits
            // `dy` lower than Slint's line box, so the ink lands exactly on the
            // engine's (x, y).
            let left = (x - PAD) as f32;
            let top = (y - PAD - located.dy) as f32;
            let right = left + located.w as f32;
            let bottom = top + located.h as f32;
            // The sampler takes normalised uv, and the atlas is one page big:
            // texel -> uv is a single divide per edge.
            let u0 = located.x as f32 / PAGE_W as f32;
            let v0 = located.y as f32 / PAGE_H as f32;
            let u1 = (located.x + located.w) as f32 / PAGE_W as f32;
            let v1 = (located.y + located.h) as f32 / PAGE_H as f32;
            let at = |x: f32, y: f32, u: f32, v: f32| Vertex {
                x,
                y,
                u,
                v,
                color: shade,
            };
            self.scratch.push((
                located.page,
                [
                    at(left, top, u0, v0),
                    at(right, top, u1, v0),
                    at(right, bottom, u1, v1),
                    at(left, top, u0, v0),
                    at(right, bottom, u1, v1),
                    at(left, bottom, u0, v1),
                ],
            ));
        }
        self.dirty = false;
        self.target = target;
        self.quads = self.scratch.len() as u32;
        // Pages go out as separate draws; this sort is 200 swaps at most.
        self.scratch.sort_unstable_by_key(|(page, _)| *page);
        let drawn = self.draw(&layer.swap, target);
        // Always hand the buffer back, even if the draw failed: the wait above
        // took the compositor's signal, and only a present re-arms it - skipping
        // it once would leave this layer permanently unable to draw until the
        // next resize (measured: `dm_s` dropped to 0 and stayed there).
        let presented = layer
            .present_backbuffer()
            .map(|hr| hr.0 == 0)
            .unwrap_or(false);
        drawn?;
        let us = started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
        self.samples.push(us);
        self.log(started.elapsed().as_micros(), target);
        Ok(Some(Frame { us, presented }))
    }

    fn log(&mut self, total_us: u128, target: (u32, u32)) {
        self.logged += 1;
        // The first frames and then every two seconds at 30 Hz: a cache that
        // never hits, or an atlas that only grows, is a bug and has to show up
        // in the log without a debugger.
        if self.logged > 5 && self.logged % 60 != 0 {
            return;
        }
        let (p50, p95) = self.samples.percentiles();
        crate::debug_log(&format!(
            "danmaku paint n={} strips={} size={}x{} quads={} compose_us p50={p50} p95={p95} \
             total_us={total_us} hits={} misses={} evict={} pages={} atlas_mb={:.1}",
            self.logged,
            self.map.len(),
            target.0,
            target.1,
            self.quads,
            self.hits,
            self.misses,
            self.evictions,
            self.pages.len(),
            self.bytes as f64 / (1024.0 * 1024.0)
        ));
    }

    /// The mask for `(dmid, size, bold)`, rasterising and uploading it the
    /// first time it is seen.
    fn strip(
        &mut self,
        dmid: &slint::SharedString,
        text: &slint::SharedString,
        size: u32,
        bold: bool,
    ) -> Option<Located> {
        let key: Key = (dmid.clone(), size, bold);
        self.stamp += 1;
        let stamp = self.stamp;
        if let Some(located) = self.map.get_mut(&key) {
            if let Some(page) = self.pages.get_mut(located.page as usize) {
                page.used = stamp;
            }
            let located = *located;
            self.hits += 1;
            return Some(located);
        }
        let mask = match self.raster.render(text.as_str(), size, bold) {
            Ok(mask) => mask,
            Err(error) => {
                crate::debug_log(&format!("danmaku strips raster failed: {error}"));
                // Remember the failure so a bad string is not retried 60 times
                // a second; a zero-size rect draws nothing.
                self.map.insert(
                    key,
                    Located {
                        page: 0,
                        x: 0,
                        y: 0,
                        w: 0,
                        h: 0,
                        dy: 0,
                    },
                );
                return None;
            }
        };
        let located = self.place(mask.width, mask.height, mask.dy)?;
        let region = D3D11_BOX {
            left: located.x,
            top: located.y,
            front: 0,
            right: located.x + located.w,
            bottom: located.y + located.h,
            back: 1,
        };
        // SAFETY: the box is inside the page texture, and `pixels` is the
        // tightly packed mask for exactly that box (`UpdateSubresource` takes
        // the box width as the source row pitch).
        unsafe {
            let page = &self.pages[located.page as usize];
            self.context.UpdateSubresource(
                &page.texture,
                0,
                Some(&region),
                self.raster.pixels().as_ptr() as *const c_void,
                located.w,
                located.w * located.h,
            );
        }
        self.map.insert(key, located);
        self.misses += 1;
        if self.misses <= 5 || self.misses % 200 == 0 {
            crate::debug_log(&format!(
                "danmaku strips mask n={} size={}x{} em={}px bold={bold} atlas_mb={:.1} \
                 pages={} entries={} evict={}",
                self.misses,
                located.w,
                located.h,
                size,
                self.bytes as f64 / (1024.0 * 1024.0),
                self.pages.len(),
                self.map.len(),
                self.evictions
            ));
        }
        Some(located)
    }

    /// Shelf-pack `w`x`h` into a page, growing the atlas or evicting the
    /// coldest page first.
    fn place(&mut self, w: u32, h: u32, dy: i32) -> Option<Located> {
        if w == 0 || h == 0 || w > PAGE_W || h > PAGE_H {
            return None;
        }
        if let Some(index) = self.pages.len().checked_sub(1) {
            if let Some(located) = self.shelve(index, w, h, dy) {
                return Some(located);
            }
        }
        // A new page only while the budget allows it; otherwise the coldest
        // page goes and its memory is reused.
        if self.bytes + PAGE_BYTES > self.cap && self.evict_page().is_none() {
            return None;
        }
        let index = self.new_page()?;
        self.shelve(index, w, h, dy)
    }

    fn shelve(&mut self, index: usize, w: u32, h: u32, dy: i32) -> Option<Located> {
        let page = self.pages.get_mut(index)?;
        // Shelf packer: rows, 1 px of gutter so a neighbouring mask cannot
        // bleed in through any future filtering.
        let fits = page.shelf_h > 0 && h <= page.shelf_h && page.shelf_x + w <= PAGE_W;
        if !fits {
            if page.shelf_h > 0 {
                page.shelf_y += page.shelf_h + 1;
            }
            page.shelf_h = h;
            page.shelf_x = 0;
        }
        if page.shelf_x + w > PAGE_W || page.shelf_y + h > PAGE_H {
            return None;
        }
        let located = Located {
            page: index as u32,
            x: page.shelf_x,
            y: page.shelf_y,
            w,
            h,
            dy,
        };
        page.shelf_x += w + 1;
        Some(located)
    }

    fn new_page(&mut self) -> Option<usize> {
        let description = D3D11_TEXTURE2D_DESC {
            Width: PAGE_W,
            Height: PAGE_H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        // SAFETY: same device as the swapchain; every page is the same size and
        // the budget was checked before the call.
        let created = unsafe {
            let mut texture: Option<ID3D11Texture2D> = None;
            let mut view: Option<ID3D11ShaderResourceView> = None;
            if self
                .device
                .CreateTexture2D(&description, None, Some(&mut texture))
                .is_err()
            {
                return None;
            }
            let Some(texture) = texture else {
                return None;
            };
            if self
                .device
                .CreateShaderResourceView(&texture, None, Some(&mut view))
                .is_err()
            {
                return None;
            }
            view.map(|view| (texture, view))
        };
        let (texture, view) = created?;
        self.pages.push(Page {
            texture,
            view,
            shelf_x: 0,
            shelf_y: 0,
            shelf_h: 0,
            used: self.stamp,
        });
        self.bytes += PAGE_BYTES;
        self.pages.len().checked_sub(1)
    }

    /// Drop the least recently used page and everything cached on it.
    fn evict_page(&mut self) -> Option<()> {
        if self.pages.len() <= 1 {
            // One page and it is still too small: clear its entries and let the
            // shelf packer start from the top again.
            if self.pages.is_empty() {
                return None;
            }
            self.map.clear();
            let page = &mut self.pages[0];
            page.shelf_x = 0;
            page.shelf_y = 0;
            page.shelf_h = 0;
            self.evictions += 1;
            crate::debug_log("danmaku strips evict lone page entries");
            return Some(());
        }
        let coldest = self
            .pages
            .iter()
            .enumerate()
            .min_by_key(|(_, page)| page.used)
            .map(|(index, _)| index)?;
        self.pages.remove(coldest);
        self.bytes = self.bytes.saturating_sub(PAGE_BYTES);
        self.map.retain(|_, located| located.page as usize != coldest);
        for located in self.map.values_mut() {
            if located.page as usize > coldest {
                located.page -= 1;
            }
        }
        self.evictions += 1;
        crate::debug_log(&format!(
            "danmaku strips evict page={coldest} pages={} entries={} atlas_mb={:.1}",
            self.pages.len(),
            self.map.len(),
            self.bytes as f64 / (1024.0 * 1024.0)
        ));
        Some(())
    }

    fn draw(&mut self, swap: &IDXGISwapChain1, target: (u32, u32)) -> Result<(), String> {
        self.flat.clear();
        for (_, quad) in &self.scratch {
            self.flat.extend_from_slice(quad);
        }
        let verts = self.flat.len();
        // The vertex buffer is allocated once with a fixed capacity and never
        // replaced: a buffer that is still referenced by the pipeline when it is
        // dropped is a dangling reference the D3D11 runtime faults on (measured:
        // growing it between frames crashed inside `d3d11.dll`). 8192 vertices is
        // 1365 quads - six times the busiest 1080p frame measured here.
        if verts > self.capacity {
            if !self.truncated {
                self.truncated = true;
                crate::debug_log(&format!(
                    "danmaku strips truncated quads={} capacity={}",
                    verts / 6,
                    self.capacity / 6
                ));
            }
            self.flat.truncate(self.capacity - self.capacity % 6);
        }
        let verts = self.flat.len();
        // f210: make sure the offscreen target matches the swapchain, then draw
        // into it. The backbuffer is only the destination of a `CopyResource`
        // at the end - no `ClearRenderTargetView`, no draw and no render-target
        // view of it, which is what the driver faulted on when this path drew
        // straight into the flip-model buffer.
        self.ensure_target(target)?;
        let Some(target_rtv) = self.target_rtv.clone() else {
            return Err("no offscreen render target".into());
        };
        // SAFETY: every object belongs to the device the layer presents with,
        // and the backbuffer reference is dropped before the caller presents
        // (see `warp.rs::draw` for why holding it breaks a flip-model present).
        unsafe {
            // Transparent: the layer swapchain is ALPHA_MODE_PREMULTIPLIED, so
            // "nothing here" must be (0, 0, 0, 0) and not black. The clear goes
            // to the offscreen texture, which is the whole point of f210.
            self.context
                .ClearRenderTargetView(&target_rtv, &[0.0, 0.0, 0.0, 0.0]);
            super::f210::note_clear(super::f210::DANMAKU);
            if verts > 0 {
                let vertex_bytes = self.capacity * std::mem::size_of::<Vertex>();
                self.upload.clear();
                self.upload.resize(vertex_bytes, 0);
                let used = std::mem::size_of_val(self.flat.as_slice());
                // SAFETY: `flat` holds `verts <= capacity` vertices, so `used`
                // is at most `vertex_bytes`; both are plain `Vertex` bytes.
                let flat = std::slice::from_raw_parts(self.flat.as_ptr() as *const u8, used);
                self.upload[..used].copy_from_slice(flat);
                self.context.UpdateSubresource(
                    &self.vertices,
                    0,
                    None,
                    self.upload.as_ptr() as *const c_void,
                    0,
                    0,
                );
                super::f210::note_update(super::f210::DANMAKU, vertex_bytes as u64);
                let packed = [2.0 / target.0 as f32, 2.0 / target.1 as f32, 0.0, 0.0];
                self.context.UpdateSubresource(
                    &self.params,
                    0,
                    None,
                    packed.as_ptr() as *const c_void,
                    0,
                    0,
                );
                super::f210::note_update(super::f210::DANMAKU, 16);
                let viewport = D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: target.0 as f32,
                    Height: target.1 as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                };
                self.context.RSSetViewports(Some(&[viewport]));
                self.context.OMSetRenderTargets(
                    Some(&[Some(target_rtv)]),
                    None::<&ID3D11DepthStencilView>,
                );
                self.context.OMSetBlendState(&self.blend, None, u32::MAX);
                self.context.RSSetState(&self.raster_state);
                self.context
                    .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                self.context.IASetInputLayout(&self.input);
                let buffers = [Some(self.vertices.clone())];
                let stride = std::mem::size_of::<Vertex>() as u32;
                let offset = 0u32;
                self.context.IASetVertexBuffers(
                    0,
                    1,
                    Some(buffers.as_ptr()),
                    Some(&stride),
                    Some(&offset),
                );
                self.context.VSSetShader(&self.vs, None);
                self.context.PSSetShader(&self.ps, None);
                self.context
                    .VSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
                self.context
                    .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
                // One draw per atlas page, in page order.
                let mut start = 0usize;
                while start < self.scratch.len() {
                    let page = self.scratch[start].0 as usize;
                    let mut end = start + 1;
                    while end < self.scratch.len() && self.scratch[end].0 as usize == page {
                        end += 1;
                    }
                    if let Some(page) = self.pages.get(page) {
                        self.context
                            .PSSetShaderResources(0, Some(&[Some(page.view.clone())]));
                        self.context
                            .Draw(((end - start) * 6) as u32, (start * 6) as u32);
                        super::f210::note_draw(super::f210::DANMAKU);
                    }
                    start = end;
                }
            }
            // Unbind the atlas and the render target, and always, not only on a
            // frame that drew: a texture left bound when the next mask is
            // uploaded through `UpdateSubresource` is a hazard (measured:
            // `nvwgf2umx` access violation after tens of frames, same driver
            // offset every time). Shaders and samplers stay bound between
            // frames - re-creating that state every frame cost ~0.4 ms of the
            // compose budget for nothing.
            self.context
                .PSSetShaderResources(0, Some(&[None::<ID3D11ShaderResourceView>]));
            self.context
                .OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
            // f210: the backbuffer is written by one copy, right here, and the
            // reference is dropped before the caller presents.
            let back: ID3D11Texture2D = swap
                .GetBuffer(0)
                .map_err(|error| format!("GetBuffer(0) hr={error}"))?;
            if let Some(tex) = self.target_tex.as_ref() {
                self.context.CopyResource(&back, tex);
                super::f210::note_copy(super::f210::DANMAKU, 1);
            }
            drop(back);
        }
        Ok(())
    }

    /// f210: the offscreen target, created once per size.
    ///
    /// A `D3D11_BIND_RENDER_TARGET` texture the same size as the swapchain, the
    /// same format and no mips, so `CopyResource` is a whole-resource copy. Its
    /// view is kept for the life of the size: creating a view per frame (what
    /// this path used to do on the backbuffer) is both waste and a reference to
    /// a flip-model surface held across a present.
    fn ensure_target(&mut self, target: (u32, u32)) -> Result<(), String> {
        if self.target_size == target && self.target_rtv.is_some() {
            return Ok(());
        }
        self.target_rtv = None;
        self.target_tex = None;
        let description = D3D11_TEXTURE2D_DESC {
            Width: target.0,
            Height: target.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        let mut view: Option<ID3D11RenderTargetView> = None;
        // SAFETY: a texture and its default view on this compositor's device.
        unsafe {
            self.device
                .CreateTexture2D(&description, None, Some(&mut tex))
                .map_err(|error| format!("CreateTexture2D offscreen hr={error}"))?;
            let Some(ref texture) = tex else {
                return Err("no offscreen texture".into());
            };
            self.device
                .CreateRenderTargetView(texture, None, Some(&mut view))
                .map_err(|error| format!("CreateRenderTargetView offscreen hr={error}"))?;
            if view.is_none() {
                return Err("no offscreen render target view".into());
            }
        }
        self.target_tex = tex;
        self.target_rtv = view;
        self.target_size = target;
        crate::debug_log(&format!(
            "danmaku strips offscreen target {}x{} (backbuffer is copy-only)",
            target.0, target.1
        ));
        Ok(())
    }

}

/// The blit shader: pixel -> clip, one coverage sample, premultiplied out.
fn shader() -> Result<(Vec<u8>, Vec<u8>), String> {
    static BYTECODE: std::sync::OnceLock<Result<(Vec<u8>, Vec<u8>), String>> =
        std::sync::OnceLock::new();
    BYTECODE
        .get_or_init(|| {
            let vs = warp::compile(SHADER, "vs_main", "vs_5_0")?;
            let ps = warp::compile(SHADER, "ps_main", "ps_5_0")?;
            Ok((vs, ps))
        })
        .clone()
}

const SHADER: &str = r#"
cbuffer Frame : register(b0)
{
    // x: 2/width, y: 2/height (the pixel -> clip scale), z/w unused.
    float4 scale;
};

struct VSIn
{
    float2 pos : POSITION;
    float2 uv  : TEXCOORD0;
    float4 color : COLOR0;
};

struct VSOut
{
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
    float4 color : COLOR0;
};

VSOut vs_main(VSIn input)
{
    VSOut o;
    // Top-left origin, y down: the same space the danmaku engine uses.
    o.pos = float4(input.pos.x * scale.x - 1.0, 1.0 - input.pos.y * scale.y, 0.0, 1.0);
    o.uv = input.uv;
    o.color = input.color;
    return o;
}

// The mask carries coverage only; the colour is the per-item constant, so a
// comment changing opacity or colour never re-rasterises.
Texture2D<float> mask : register(t0);
SamplerState samp : register(s0);

float4 ps_main(VSOut input) : SV_Target
{
    float coverage = mask.SampleLevel(samp, input.uv, 0.0);
    float alpha = coverage * input.color.a;
    // Premultiplied: the layer swapchain is ALPHA_MODE_PREMULTIPLIED.
    return float4(input.color.rgb * alpha, alpha);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The shelf packer must never hand out overlapping rects and must report
    /// failure instead of wrapping off the page.
    #[test]
    fn shelf_packing_is_exact() {
        let page_w = 64u32;
        let page_h = 40u32;
        let (mut shelf_x, mut shelf_y, mut shelf_h) = (0u32, 0u32, 0u32);
        let mut rects: Vec<(u32, u32, u32, u32)> = Vec::new();
        for index in 0..9u32 {
            let (w, h) = (20u32, 8u32 + index % 3);
            // Mirror of `shelve`, with `page_w` standing in for `PAGE_W`.
            let fits = shelf_h > 0 && h <= shelf_h && shelf_x + w <= page_w;
            if !fits {
                if shelf_h > 0 {
                    shelf_y += shelf_h + 1;
                }
                shelf_h = h;
                shelf_x = 0;
            }
            if shelf_x + w > page_w || shelf_y + h > page_h {
                break;
            }
            rects.push((shelf_x, shelf_y, w, h));
            shelf_x += w + 1;
        }
        assert!(rects.len() >= 3, "packer stopped early: {}", rects.len());
        for (index, a) in rects.iter().enumerate() {
            for b in rects.iter().skip(index + 1) {
                let overlap =
                    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3;
                assert!(!overlap, "{a:?} overlaps {b:?}");
            }
        }
    }

    /// A percentile ring ignores zero slots and never reports a value that was
    /// not sampled.
    #[test]
    fn samples_percentiles_ignore_empty_slots() {
        let mut samples = Samples::new(8);
        for value in [10u32, 20, 30, 40] {
            samples.push(value);
        }
        let (p50, p95) = samples.percentiles();
        assert!((10..=40).contains(&p50), "p50={p50}");
        assert!((p50..=40).contains(&p95), "p95={p95}");
        assert_eq!(Samples::new(4).percentiles(), (0, 0));
    }

    /// The GDI rasteriser, on its own: fonts installed from the bundled bytes,
    /// a 52 px bold run at 1080p scale. Ignored by default (`--ignored`) because
    /// it touches the window station; run it when the mask path is suspect:
    ///
    /// ```text
    /// cargo test --release rasterises_a_bundled_face -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn rasterises_a_bundled_face() {
        let mut raster = Raster::new().expect("fonts");
        println!("fonts={}", raster.face);
        let (upem, typo, hhea) = face_metrics(REGULAR);
        println!("font upem={upem} typo_ascender={typo} hhea_ascender={hhea}");
        let started = std::time::Instant::now();
        for _ in 0..5 {
            let _ = raster.render("弹幕测试 abc 123", 32, false);
        }
        println!("5 warm renders in {:?}", started.elapsed());
        for size in [27u32, 32, 40, 52] {
            for bold in [false, true] {
                let t0 = std::time::Instant::now();
                let mask = raster
                    .render("弹幕测试 abc 123", size, bold)
                    .unwrap_or_else(|error| panic!("size={size} bold={bold}: {error}"));
                let _elapsed = t0.elapsed();
                let ink = raster.pixels().iter().filter(|p| **p > 0).count();
                let mut top = usize::MAX;
                let width = mask.width as usize;
                for (index, coverage) in raster.pixels().iter().enumerate() {
                    if *coverage > 0 {
                        top = (index / width).min(top);
                    }
                }
                println!(
                    "size={size} bold={bold} mask={}x{} ink={ink} ink_top={top} dy={} \
                     ascent_px={:.2}",
                    mask.width,
                    mask.height,
                    mask.dy,
                    typo as f32 * size as f32 / upem as f32
                );
                assert!(ink > 0, "no coverage at size={size} bold={bold}");
            }
        }
    }
}
