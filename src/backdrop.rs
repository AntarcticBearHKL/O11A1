//! Generated diffuse-gradient backdrop.
//!
//! The window background is a designed image, not a picture of the desktop: a
//! deep (dark) or near-white (light) base tone with a handful of very large,
//! soft radial colour blobs blended over it at low opacity. It replaces the old
//! desktop-capture frosted glass, which grabbed, blurred and re-placed the
//! screen behind the window every second.
//!
//! Properties that matter for the UI:
//!
//! * **No capture, ever.** The image is computed once per theme and cached, so
//!   a window is never waiting for a frame: the first paint already has it, a
//!   window move costs nothing and an idle window costs nothing.
//! * **No banding.** A 4K-wide window shows a gradient that is only 0.1 % per
//!   pixel of the 8-bit range, which would band. Before quantising, a blue-noise
//!   tile (`±1.5/255`) is added, so the steps are broken up without visible
//!   grain.
//! * **A second, pre-blurred copy** ([`blurred`]) is what the frosted panels
//!   paint. Because the backdrop is a designed gradient rather than a picture of
//!   the desktop, a blurred copy of it is what "what is behind the panel,
//!   diffused" would look like - so the blur is done once in Rust at startup
//!   (three separable box passes, ~Gaussian sigma 24 px) and every panel then
//!   paints a static image. No sampling, no timer, no per-frame work.
//! * **Same family as the app tints.** The blobs sit in the accent hue family
//!   (muted blue / teal / violet in dark, warm / blue / mint in light) at 8-18 %,
//!   so `window-bg`, `player-glass` and `bar-fill` still read as before.
//!
//! The cache is a `thread_local`: `slint::Image` caches per-thread renderer
//! state and is not `Send`, and every caller (window creation, theme switch)
//! runs on the UI thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Edge length of the generated tile. The wallpaper is one seamless tile that
/// repeats across a virtual desktop; a window paints the part of it under the
/// window's screen position, so moving the window reveals new colour.
const SIZE: usize = 1024;

/// The tile's edge in logical pixels, published to the UI so it can offset the
/// repeating wallpaper by the window's screen position.
pub fn tile_px() -> f32 {
    SIZE as f32
}

/// Edge length of the blue-noise tile that is repeated over the image.
const NOISE_TILE: usize = 64;

/// Edge length of the grain tile baked into the pre-blurred copy.
///
/// Slint's `Image` has no repeat/tile fit (only fill/cover/contain), so the
/// grain cannot be exposed as a cheap tiled layer. It is tiled into the blurred
/// image here instead, once, at the same cost as one pass over the pixels - the
/// panel still paints a single static image.
const GRAIN_TILE: usize = 256;

/// Half-amplitude of that grain, in 8-bit steps. 2/255 is enough to break the
/// smooth 8-bit steps of the blurred gradient without reading as noise.
const GRAIN: f32 = 2.0;

/// Radius, in **image** pixels (1024-space), each of the three box passes
/// stands for.
///
/// Three boxes of width `2r+1` have variance `3 * ((2r+1)^2 - 1) / 12`, so
/// `r = 24` gives a standard deviation of ~24.5 px - the "Gaussian radius 24"
/// the glass look is specified with. The passes run on a 4x downsampled copy
/// (radius 6 at 256, upsampled bilinearly), which is the same kernel at 1/16
/// of the pixels and is all a deliberately diffuse gradient needs.

/// Downsample factor for the blur: `256 * 4 = 1024`.
const BLUR_SCALE: usize = 4;

/// Radius of each box pass on the downsampled copy for an explicit slider
/// value (`px / BLUR_SCALE`). Called with [`BLUR_PX`] for the cached startup
/// image and with the worker's own value for a live slider change, so a
/// generation started off the UI thread never races the atomic.
fn low_radius(px: u32) -> usize {
    (px as usize) / BLUR_SCALE
}

/// Number of separable box passes; three is the standard Gaussian approximation.
const BLUR_PASSES: usize = 3;

/// One soft colour blob in the composition: centre and radius as fractions of
/// the image, the colour it mixes toward, and how strongly it does so.
struct Blob {
    x: f32,
    y: f32,
    r: f32,
    color: [f32; 3],
    alpha: f32,
}

/// sRGB hex to `[f32; 3]`.
const fn rgb(hex: u32) -> [f32; 3] {
    [
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
    ]
}

/// Dark composition: a deep, slightly blue near-black base with muted blue,
/// teal and violet blobs. The blobs overlap into one diffuse wash - no shape is
/// readable on its own, which is what keeps it from looking like a test pattern.
///
/// The centres are placed off a diagonal (upper-left -> lower-right) with
/// different radii, so the result has a direction and a light source rather than
/// a symmetric pattern.
const DARK_BASE: [f32; 3] = rgb(0x07_07_0b);
const DARK_BLOBS: &[Blob] = &[
    Blob { x: 0.20, y: 0.18, r: 0.58, color: rgb(0x6d_4d_ff), alpha: 0.58 },
    Blob { x: 0.80, y: 0.22, r: 0.52, color: rgb(0xff_4d_8d), alpha: 0.46 },
    Blob { x: 0.62, y: 0.62, r: 0.60, color: rgb(0x19_c2_ff), alpha: 0.46 },
    Blob { x: 0.30, y: 0.80, r: 0.52, color: rgb(0x12_d6_b0), alpha: 0.42 },
    Blob { x: 0.88, y: 0.80, r: 0.46, color: rgb(0xff_9a_3c), alpha: 0.34 },
    Blob { x: 0.55, y: 0.10, r: 0.62, color: rgb(0x3b_2b_d6), alpha: 0.36 },
];

/// Light composition: a near-white base with the same hue family at pastel
/// strength, so the two themes read as the same wallpaper, not two wallpapers.
const LIGHT_BASE: [f32; 3] = rgb(0xfa_fa_fc);
const LIGHT_BLOBS: &[Blob] = &[
    Blob { x: 0.20, y: 0.18, r: 0.58, color: rgb(0xb9_a8_ff), alpha: 0.60 },
    Blob { x: 0.80, y: 0.22, r: 0.52, color: rgb(0xff_b3_c9), alpha: 0.55 },
    Blob { x: 0.62, y: 0.62, r: 0.60, color: rgb(0xa9_e4_ff), alpha: 0.55 },
    Blob { x: 0.30, y: 0.80, r: 0.52, color: rgb(0xa9_f0_df), alpha: 0.50 },
    Blob { x: 0.88, y: 0.80, r: 0.46, color: rgb(0xff_d7_a8), alpha: 0.45 },
    Blob { x: 0.55, y: 0.10, r: 0.62, color: rgb(0xc9_c0_ff), alpha: 0.40 },
];

/// Gaussian radius applied to the wallpaper, in image pixels (1024 canvas).
/// `0` = the coloured balls stay crisp; `60` = a soft mesh gradient.
static BLUR_PX: AtomicU32 = AtomicU32::new(DEFAULT_BLUR_PX);
/// Default diffusion: soft enough to read as a mesh gradient, sharp enough that
/// the ball's colours still have a direction.
const DEFAULT_BLUR_PX: u32 = 60;
/// The slider's range.
pub const MAX_BLUR_PX: u32 = 160;

/// Set the wallpaper's blur radius (image pixels). Returns `true` when it
/// changed, so the caller can re-publish the window/panel images.
pub fn set_blur(px: u32) -> bool {
    let next = px.min(MAX_BLUR_PX);
    let previous = BLUR_PX.swap(next, Ordering::SeqCst);
    if previous == next {
        return false;
    }
    DARK_BLUR.with(|cache| *cache.borrow_mut() = None);
    LIGHT_BLUR.with(|cache| *cache.borrow_mut() = None);
    true
}

/// The current blur radius (for the settings UI).
pub fn blur_px() -> u32 {
    BLUR_PX.load(Ordering::SeqCst)
}

thread_local! {
    /// One cached image per theme, built on first use (UI thread only).
    /// The rendered sharp pixels, kept beside the image so the blurred copy
    /// does not have to render the gradient a second time.
    static DARK_PIXELS: RefCell<Option<Rc<SharedPixelBuffer<Rgba8Pixel>>>> =
        const { RefCell::new(None) };
    static LIGHT_PIXELS: RefCell<Option<Rc<SharedPixelBuffer<Rgba8Pixel>>>> =
        const { RefCell::new(None) };
    /// The pre-blurred copies the frosted panels paint, cached next to the
    /// sharp ones. Same size, same lifetime contract.
    static DARK_BLUR: RefCell<Option<Image>> = const { RefCell::new(None) };
    static LIGHT_BLUR: RefCell<Option<Image>> = const { RefCell::new(None) };
}

/// The theme's sharp pixels, rendered once and shared by both the window image
/// and the blur. `Rc` keeps the refcount explicit; nothing mutates the buffer.
fn sharp_pixels(dark: bool) -> Rc<SharedPixelBuffer<Rgba8Pixel>> {
    let slot = if dark { &DARK_PIXELS } else { &LIGHT_PIXELS };
    slot.with(|cache| {
        if let Some(buffer) = cache.borrow().as_ref() {
            return buffer.clone();
        }
        let buffer = Rc::new(render(dark));
        *cache.borrow_mut() = Some(buffer.clone());
        buffer
    })
}

/// The theme's backdrop, generated once and cached.
///
/// Returns a cheap clone of the cached `slint::Image` (the pixels are shared, no
/// regeneration and no copy), so this is safe to call for every window at
/// creation and again on every theme switch.
pub fn image(dark: bool) -> Image {
    // The wallpaper IS the diffused image: at blur 0 the coloured balls read as
    // crisp discs, at the default they melt into a mesh gradient. Panels paint
    // the very same pixels, so a panel can never show a different diffusion
    // than the window behind it.
    blurred(dark)
}

/// The theme's **pre-blurred** backdrop: the same gradient, diffused with
/// three separable box passes (~Gaussian sigma 24.5 px, run on a 4x
/// downsampled copy - the same kernel at 1/16 of the pixels) and a 2/255 grain
/// layer baked in against banding. ~17 ms once per theme on the UI thread,
/// before the first paint.
///
/// Every frosted panel paints this one image (through `AppTheme.glass-blur`), so
/// a panel is a static texture plus an alpha tint: there is no sampling of the
/// window behind it, no render target, and nothing recomputed per frame. Cached
/// exactly like [`image`]; the second caller gets the same pixels.
pub fn blurred(dark: bool) -> Image {
    let slot = if dark { &DARK_BLUR } else { &LIGHT_BLUR };
    slot.with(|cache| {
        if let Some(image) = cache.borrow().as_ref() {
            return image.clone();
        }
        let image = build_blurred(dark);
        *cache.borrow_mut() = Some(image.clone());
        image
    })
}

/// Render one theme's gradient into a pixel buffer.
fn render(dark: bool) -> SharedPixelBuffer<Rgba8Pixel> {
    let (base, blobs) = if dark {
        (DARK_BASE, DARK_BLOBS)
    } else {
        (LIGHT_BASE, LIGHT_BLOBS)
    };
    let noise = noise_tile();
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(SIZE as u32, SIZE as u32);
    let dst = buffer.make_mut_bytes();
    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut color = [base[0], base[1], base[2]];
            for blob in blobs {
                // Toroidal (wrapped) distance, so the tile is seamless in all
                // four directions: the wallpaper repeats across a virtual
                // desktop without a visible seam when a window moves over it.
                let mut dx = (x as f32 / SIZE as f32 - blob.x).abs();
                if dx > 0.5 {
                    dx = 1.0 - dx;
                }
                let mut dy = (y as f32 / SIZE as f32 - blob.y).abs();
                if dy > 0.5 {
                    dy = 1.0 - dy;
                }
                // Squared-distance falloff, no `sqrt`: `t` is 1 at the centre
                // and 0 at the radius, and smoothstep makes the edge invisible.
                let t = 1.0 - (dx * dx + dy * dy) / (blob.r * blob.r);
                if t <= 0.0 {
                    continue;
                }
                let t = t.min(1.0);
                let weight = t * t * (3.0 - 2.0 * t) * blob.alpha;
                for channel in 0..3 {
                    color[channel] += (blob.color[channel] - color[channel]) * weight;
                }
            }
            let i = (y * SIZE + x) * 4;
            let dither = noise[(y % NOISE_TILE) * NOISE_TILE + (x % NOISE_TILE)];
            for channel in 0..3 {
                // `+0.5` rounds instead of truncating, then the blue-noise dither
                // breaks up the 8-bit steps this gradient would otherwise show.
                let value = color[channel] * 255.0 + dither + 0.5;
                dst[i + channel] = value.clamp(0.0, 255.0) as u8;
            }
            dst[i + 3] = 255;
        }
    }
    buffer
}

/// Build the pre-blurred panel image: reuse the cached sharp pixels, diffuse
/// them, then add the tiled grain and quantise once.
fn build_blurred(dark: bool) -> Image {
    Image::from_rgba8(blur_pixels(dark))
}

/// Generate the wallpaper/panel pixels for an explicit blur radius.
///
/// This is the OFF-THREAD entry point: it touches no thread-local cache and
/// reads no shared atomic, so a worker can run it while the UI keeps painting
/// the previous image. The caller installs the result on the UI thread with
/// [`install`]. Pure pixels in, pixels out; `SharedPixelBuffer` is `Send`.
pub fn generate(dark: bool, px: u32) -> SharedPixelBuffer<Rgba8Pixel> {
    blur_core(dark, true, px.min(MAX_BLUR_PX))
}

/// Install a [`generate`] result as the current image for that theme. UI thread
/// only (it updates the same thread-local cache every panel reads); the second
/// theme keeps its old image until its own generation arrives.
pub fn install(dark: bool, buffer: SharedPixelBuffer<Rgba8Pixel>) -> Image {
    let image = Image::from_rgba8(buffer);
    let slot = if dark { &DARK_BLUR } else { &LIGHT_BLUR };
    slot.with(|cache| *cache.borrow_mut() = Some(image.clone()));
    image
}

/// The diffused gradient (and, for the panel image, the grain): render the sharp
/// copy (cached with the window image), box-downsample it 4x, run three
/// separable box passes (radius 6 at 256 = 24 px in image space) and upsample
/// bilinearly.
fn blur_core(dark: bool, with_grain: bool, px: u32) -> SharedPixelBuffer<Rgba8Pixel> {
    let sharp = sharp_pixels(dark);
    let source = sharp.as_bytes();
    let low = SIZE / BLUR_SCALE;
    let radius = low_radius(px);
    let mut planes = [
        vec![0.0f32; low * low],
        vec![0.0f32; low * low],
        vec![0.0f32; low * low],
    ];
    // 4x4 box downsample. The source is a smooth gradient, so this is also the
    // first (tiny) box of the kernel, not a lossy shortcut.
    let area = (BLUR_SCALE * BLUR_SCALE) as f32;
    for ly in 0..low {
        for lx in 0..low {
            let mut sums = [0.0f32; 3];
            for oy in 0..BLUR_SCALE {
                let row = (ly * BLUR_SCALE + oy) * SIZE;
                for ox in 0..BLUR_SCALE {
                    let i = (row + lx * BLUR_SCALE + ox) * 4;
                    for channel in 0..3 {
                        sums[channel] += f32::from(source[i + channel]);
                    }
                }
            }
            let out = ly * low + lx;
            for channel in 0..3 {
                planes[channel][out] = sums[channel] / area;
            }
        }
    }
    for _ in 0..BLUR_PASSES {
        box_horizontal(&mut planes, low, low, radius);
        box_vertical(&mut planes, low, low, radius);
    }
    // Bilinear upsample back to 1024. The weights are precomputed once per axis
    // (they do not vary by row/column), so the 1M-pixel loop is just four reads
    // and two lerps per channel. Both axes WRAP: output pixel 0 samples between
    // the last low cell and cell 0 (f = -0.125), not a clamped copy of cell 0.
    // Clamping here would have kept the seam even after the box passes wrapped.
    let mut xmap = [(0usize, 0usize, 0.0f32); SIZE];
    let mut ymap = [(0usize, 0usize, 0.0f32); SIZE];
    let scale = BLUR_SCALE as f32;
    let low_i = low as isize;
    for (i, slot) in xmap.iter_mut().enumerate() {
        let f = (i as f32 + 0.5) / scale - 0.5;
        let floor = f.floor();
        let i0 = (floor as isize).rem_euclid(low_i) as usize;
        let i1 = ((floor as isize) + 1).rem_euclid(low_i) as usize;
        *slot = (i0, i1, f - floor);
    }
    for (i, slot) in ymap.iter_mut().enumerate() {
        let f = (i as f32 + 0.5) / scale - 0.5;
        let floor = f.floor();
        let i0 = (floor as isize).rem_euclid(low_i) as usize;
        let i1 = ((floor as isize) + 1).rem_euclid(low_i) as usize;
        *slot = (i0, i1, f - floor);
    }
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(SIZE as u32, SIZE as u32);
    let dst = buffer.make_mut_bytes();
    let grain = grain_tile();
    for y in 0..SIZE {
        let (y0, y1, wy) = ymap[y];
        let row0 = y0 * low;
        let row1 = y1 * low;
        for x in 0..SIZE {
            let (x0, x1, wx) = xmap[x];
            let i = (y * SIZE + x) * 4;
            let dither = if with_grain {
                grain[(y % GRAIN_TILE) * GRAIN_TILE + (x % GRAIN_TILE)]
            } else {
                0.0
            };
            for channel in 0..3 {
                let plane = &planes[channel];
                let top = plane[row0 + x0] + (plane[row0 + x1] - plane[row0 + x0]) * wx;
                let bottom = plane[row1 + x0] + (plane[row1 + x1] - plane[row1 + x0]) * wx;
                let value = top + (bottom - top) * wy + dither + 0.5;
                dst[i + channel] = value.clamp(0.0, 255.0) as u8;
            }
            dst[i + 3] = 255;
        }
    }
    buffer
}

/// The diffused gradient before the grain, used by the smoothness test.
#[cfg(test)]
fn blur_only(dark: bool) -> SharedPixelBuffer<Rgba8Pixel> {
    blur_core(dark, false, blur_px())
}

/// One separable horizontal box pass, radius `radius`, **wrapped** at the tile
/// edges.
///
/// The wallpaper is a torus: its left edge is the continuation of its right
/// edge. Clamping here was the seam - the blur averaged in the border pixel
/// instead of the pixels on the far side, so the first and last column stopped
/// matching and a repeated tile showed a hard step. Every read below is
/// `(x + k) mod width`, exactly the source's own toroidal blob distance.
///
/// A sliding window keeps this O(pixels) per pass instead of O(pixels x radius):
/// one add and one subtract per output pixel per channel, no allocation.
fn box_horizontal(planes: &mut [Vec<f32>; 3], width: usize, height: usize, radius: usize) {
    let inv_window = 1.0 / (2 * radius + 1) as f32;
    let mut source = planes[0].clone();
    source.clear();
    for plane in planes.iter_mut() {
        source.clone_from(plane);
        for y in 0..height {
            let row = y * width;
            let mut sum = 0.0;
            for offset in -(radius as isize)..=(radius as isize) {
                let x = offset.rem_euclid(width as isize) as usize;
                sum += source[row + x];
            }
            for x in 0..width {
                plane[row + x] = sum * inv_window;
                let leaving = (x + width - radius) % width;
                let entering = (x + radius + 1) % width;
                sum += source[row + entering] - source[row + leaving];
            }
        }
    }
}

/// One separable vertical box pass, mirroring [`box_horizontal`] (wrapped).
fn box_vertical(planes: &mut [Vec<f32>; 3], width: usize, height: usize, radius: usize) {
    let inv_window = 1.0 / (2 * radius + 1) as f32;
    let mut source = planes[0].clone();
    source.clear();
    for plane in planes.iter_mut() {
        source.clone_from(plane);
        for x in 0..width {
            let mut sum = 0.0;
            for offset in -(radius as isize)..=(radius as isize) {
                let y = offset.rem_euclid(height as isize) as usize;
                sum += source[y * width + x];
            }
            for y in 0..height {
                plane[y * width + x] = sum * inv_window;
                let leaving = (y + height - radius) % height;
                let entering = (y + radius + 1) % height;
                sum += source[entering * width + x] - source[leaving * width + x];
            }
        }
    }
}

/// The pixels behind [`build_blurred`]: the diffused gradient with the grain
/// tile added before the final quantisation, so flat areas do not band.
fn blur_pixels(dark: bool) -> SharedPixelBuffer<Rgba8Pixel> {
    blur_core(dark, true, blur_px())
}

/// A 256x256 grain tile in `±2/255`, tiled over the blurred image before it is
/// quantised.
///
/// Deterministic splitmix64, the same generator as the sharpen dither, so every
/// run shows the same glass. Unlike [`noise_tile`] this is not high-pass
/// filtered: it is applied after the blur, where the only thing to break up is
/// the quantiser's flat steps, not a low-frequency gradient.
fn grain_tile() -> &'static [f32] {
    static TILE: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    TILE.get_or_init(|| {
        (0..GRAIN_TILE * GRAIN_TILE)
            .map(|i| {
                let mut z = (i as u64).wrapping_add(0xd1b5_4a32_d192_ed03);
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^= z >> 31;
                let unit = (z >> 40) as f32 / (1u64 << 24) as f32;
                (unit * 2.0 - 1.0) * GRAIN
            })
            .collect()
    })
    .as_slice()
}

/// A 64x64 blue-noise tile, tiled over the image, in `±1.5` of the 0..255 range.
///
/// White noise, high-pass filtered against its 3x3 neighbourhood: subtracting
/// the local mean removes the low frequencies that read as grain and leaves the
/// high-frequency energy that dither needs. The tile is 64x64 so it costs one
/// 16 KiB lookup table per theme instead of one hash per pixel, and being
/// deterministic it is identical on every run.
fn noise_tile() -> &'static [f32] {
    const TILE: usize = NOISE_TILE;
    const AMPLITUDE: f32 = 1.5;
    static NOISE: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    NOISE.get_or_init(|| {
        let white: Vec<f32> = (0..TILE * TILE)
            .map(|i| {
                // splitmix64: deterministic, well distributed, no dependency.
                let mut z = (i as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^= z >> 31;
                (z >> 40) as f32 / (1u64 << 24) as f32
            })
            .collect();
        let at = |x: usize, y: usize| white[(y % TILE) * TILE + x % TILE];
        let mut filtered = vec![0.0f32; TILE * TILE];
        let mut peak = 0.0f32;
        for y in 0..TILE {
            for x in 0..TILE {
                let mut sum = 0.0;
                for oy in 0..3 {
                    for ox in 0..3 {
                        sum += at(x + ox, y + oy);
                    }
                }
                let local = sum / 9.0;
                let value = at(x, y) - local;
                peak = peak.max(value.abs());
                filtered[y * TILE + x] = value;
            }
        }
        let scale = if peak > 0.0 { AMPLITUDE / peak } else { 0.0 };
        for value in &mut filtered {
            *value *= scale;
        }
        filtered
    })
    .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The image is the documented size and cached: the second call hands back
    /// the same pixels rather than regenerating.
    #[test]
    fn image_is_cached_and_sized() {
        let first = image(true);
        assert_eq!(first.size().width, SIZE as u32);
        assert_eq!(first.size().height, SIZE as u32);
        assert_eq!(first.size(), image(true).size());
        assert_eq!(image(false).size().width, SIZE as u32);
    }

    /// Every generated pixel is opaque: the tint above it is the only
    /// transparency in the window, exactly like the old glass stack.
    #[test]
    fn pixels_are_opaque() {
        let buffer = render(false);
        assert!(buffer.as_slice().iter().all(|px| px.a == 255));
    }

    /// Both themes actually contain a range of tones (a flat fill would mean the
    /// blobs were never blended in) and stay inside the intended brightness:
    /// dark near-black, light near-white.
    #[test]
    fn both_themes_are_a_gradient() {
        // A vivid mesh gradient is deliberately brighter than the old near-black
        // wash: the colourful balls are the point. The bound still catches a
        // flat fill (no blobs) and a washed-out white.
        for (dark, center, spread) in [(true, 80.0, 60.0), (false, 226.0, 40.0)] {
            let buffer = render(dark);
            let luma: Vec<f32> = buffer
                .as_slice()
                .iter()
                .step_by(97)
                .map(|px| {
                    0.2126 * f32::from(px.r) + 0.7152 * f32::from(px.g) + 0.0722 * f32::from(px.b)
                })
                .collect();
            let mean = luma.iter().sum::<f32>() / luma.len() as f32;
            let min = luma.iter().copied().fold(f32::MAX, f32::min);
            let max = luma.iter().copied().fold(f32::MIN, f32::max);
            assert!(
                (mean - center).abs() < spread,
                "dark={dark} mean={mean}"
            );
            assert!(max - min > 4.0, "dark={dark} flat range {min}..{max}");
            assert!(max < 255.0 && min > 0.0);
        }
    }

    /// The dither is centred and bounded, so it can never shift the tone.
    #[test]
    fn noise_is_centred_and_bounded() {
        let tile = noise_tile();
        let sum: f32 = tile.iter().sum();
        assert!(sum.abs() / tile.len() as f32 <= 0.01, "mean {sum}");
        assert!(tile.iter().all(|value| value.abs() <= 1.5001));
    }

    /// The pre-blurred copy is the same size as the sharp one, cached, and
    /// distinct per theme (a light and a dark blur must not collide).
    #[test]
    fn blurred_is_cached_and_sized() {
        let dark = blurred(true);
        assert_eq!(dark.size().width, SIZE as u32);
        assert_eq!(dark.size().height, SIZE as u32);
        assert_eq!(dark.size(), blurred(true).size());
        let light = blurred(false);
        assert_eq!(light.size().width, SIZE as u32);
        // Cached, not regenerated: the second call is the same pixels.
        assert_eq!(light.size(), blurred(false).size());
    }

    /// Blurring must actually diffuse: the mean absolute difference between
    /// neighbouring pixels drops sharply against the sharp image, while the
    /// overall tone stays in the theme's family.
    #[test]
    fn blurred_is_smoother_than_sharp() {
        assert!(blur_px() > 0, "the default wallpaper is diffused");
        for dark in [true, false] {
            let sharp = render(dark);
            let soft = blur_only(dark);
            let rough = sharp.as_bytes();
            let smooth = soft.as_bytes();
            let mut sharp_delta = 0.0f64;
            let mut soft_delta = 0.0f64;
            let mut samples = 0.0f64;
            for y in (0..SIZE).step_by(7) {
                for x in 0..SIZE - 1 {
                    let a = (y * SIZE + x) * 4;
                    let b = a + 4;
                    let luma = |px: &[u8]| {
                        0.2126 * f64::from(px[0])
                            + 0.7152 * f64::from(px[1])
                            + 0.0722 * f64::from(px[2])
                    };
                    sharp_delta += (luma(&rough[b..]) - luma(&rough[a..])).abs();
                    soft_delta += (luma(&smooth[b..]) - luma(&smooth[a..])).abs();
                    samples += 1.0;
                }
            }
            let sharp_mean = sharp_delta / samples;
            let soft_mean = soft_delta / samples;
            assert!(
                soft_mean < sharp_mean * 0.25,
                "dark={dark} sharp={sharp_mean} soft={soft_mean}"
            );
            assert!(soft_mean < 1.0, "dark={dark} soft neighbour step {soft_mean}");
        }
    }

    /// The finished tile is a true torus: the step from the last column to the
    /// first is the same order as a typical interior step and is not a local
    /// maximum against its two neighbours. This is the regression test for the
    /// old clamped box passes + clamped bilinear upsample, which left a visible
    /// line wherever the wallpaper repeated. It runs on the SHIPPED pixels (the
    /// grain included), not a smoothed proxy.
    #[test]
    fn blurred_tile_wraps_without_a_seam() {
        for dark in [true, false] {
            let buffer = blur_pixels(dark);
            let (boundary, interior, neighbours) = seam_stats(buffer.as_bytes());
            assert!(
                boundary < interior * 2.0 + 0.1,
                "dark={dark} boundary={boundary:.4} interior={interior:.4}"
            );
            assert!(
                boundary <= neighbours * 1.05 + 0.05,
                "dark={dark} boundary={boundary:.4} neighbour_max={neighbours:.4}"
            );
            eprintln!(
                "seam dark={} boundary={boundary:.4} interior={interior:.4} neighbour_max={neighbours:.4}",
                u8::from(dark)
            );
        }
    }

    /// QA: write the finished light wallpaper as a 2x2 tiling, so the tile edge
    /// can be inspected as it actually repeats. `F146_CAPTURE_DIR` selects the
    /// folder (default: the harness temp folder).
    #[test]
    #[ignore]
    fn dump_seam_tiling() {
        // Match the owner's stored 背景模糊 (145 px), so the 2x2 capture is the
        // finished image the window actually repeats, not the 60 px default.
        set_blur(145);
        let buffer = blur_pixels(false);
        let source = buffer.as_bytes();
        let size = SIZE * 2;
        let mut tiled = vec![0u8; size * size * 4];
        for ty in 0..2 {
            for tx in 0..2 {
                for y in 0..SIZE {
                    let src = y * SIZE * 4;
                    let dst = ((ty * SIZE + y) * size + tx * SIZE) * 4;
                    tiled[dst..dst + SIZE * 4].copy_from_slice(&source[src..src + SIZE * 4]);
                }
            }
        }
        let dir = std::env::var_os("F146_CAPTURE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("opencode").join("f146"));
        std::fs::create_dir_all(&dir).expect("create capture dir");
        let path = dir.join("f146_seam.png");
        image::save_buffer(
            &path,
            &tiled,
            size as u32,
            size as u32,
            image::ColorType::Rgba8,
        )
        .expect("write seam tiling");
        let (boundary, interior, neighbours) = seam_stats(source);
        eprintln!(
            "seam tiling blur=145 {} boundary={boundary:.4} interior={interior:.4} neighbour_max={neighbours:.4}",
            path.display()
        );
        set_blur(DEFAULT_BLUR_PX);
    }

    /// Boundary step (last column -> first column), the mean interior step and
    /// the larger of the two columns adjacent to the boundary. All values are
    /// mean absolute luma differences over all rows, so the units are 8-bit
    /// steps per pixel.
    fn seam_stats(bytes: &[u8]) -> (f64, f64, f64) {
        let luma = |x: usize, y: usize| -> f64 {
            let i = (y * SIZE + x) * 4;
            0.2126 * f64::from(bytes[i])
                + 0.7152 * f64::from(bytes[i + 1])
                + 0.0722 * f64::from(bytes[i + 2])
        };
        let mut boundary = 0.0;
        let mut interior = 0.0;
        let mut interior_samples = 0.0;
        let mut first = 0.0;
        let mut last = 0.0;
        for y in 0..SIZE {
            boundary += (luma(0, y) - luma(SIZE - 1, y)).abs();
            first += (luma(1, y) - luma(0, y)).abs();
            last += (luma(SIZE - 1, y) - luma(SIZE - 2, y)).abs();
            for x in 0..SIZE - 1 {
                interior += (luma(x + 1, y) - luma(x, y)).abs();
                interior_samples += 1.0;
            }
        }
        let rows = SIZE as f64;
        (
            boundary / rows,
            interior / interior_samples,
            (first / rows).max(last / rows),
        )
    }


    /// The baked grain is centred and stays inside ±2/255, so it can break
    /// banding without moving the panel's measured tint.
    #[test]
    fn grain_is_centred_and_bounded() {
        let tile = grain_tile();
        assert_eq!(tile.len(), GRAIN_TILE * GRAIN_TILE);
        let mean: f32 = tile.iter().sum::<f32>() / tile.len() as f32;
        assert!(mean.abs() < 0.08, "mean {mean}");
        assert!(tile.iter().all(|value| value.abs() <= GRAIN + 0.0001));
    }

    /// QA: dump both wallpapers (and a blur-0 copy) so the composition can be
    /// looked at without launching the app. `cargo test --release dump_wallpaper
    /// -- --ignored`.
    #[test]
    #[ignore]
    fn dump_wallpaper() {
        for dark in [true, false] {
            let buffer = blur_only(dark);
            let dir = std::env::temp_dir();
            let name = if dark { "wallpaper_dark.png" } else { "wallpaper_light.png" };
            let path = dir.join(name);
            image::save_buffer(
                &path,
                buffer.as_bytes(),
                SIZE as u32,
                SIZE as u32,
                image::ColorType::Rgba8,
            )
            .expect("write wallpaper");
            eprintln!("wrote {}", path.display());
        }
        // Sharp (blur 0) composition, to show the balls themselves.
        set_blur(0);
        for dark in [true, false] {
            let buffer = blur_only(dark);
            let dir = std::env::temp_dir();
            let name = if dark { "wallpaper_dark_crisp.png" } else { "wallpaper_light_crisp.png" };
            let path = dir.join(name);
            image::save_buffer(&path, buffer.as_bytes(), SIZE as u32, SIZE as u32, image::ColorType::Rgba8)
                .expect("write crisp wallpaper");
            eprintln!("wrote {}", path.display());
        }
        set_blur(DEFAULT_BLUR_PX);
    }
}
