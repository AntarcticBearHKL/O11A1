//! Block-matching motion interpolation: the CPU alternative to frame blending.
//!
//! Blending two source frames linearly makes every moving edge appear twice at
//! half opacity. This module estimates the motion between the two frames on a
//! coarse block grid, warps the two pictures towards each other and blends the
//! *warped* pair, so a moving edge lands at one position instead of ghosting.
//!
//! Algorithm, per pair `(A, B)` at display phase `t` in `0..=1`:
//!
//! 1. Build a luma plane decimated by [`DECIMATE`] for both frames (2x2 box
//!    average, so the search is not fooled by single-pixel dither).
//! 2. For every [`BLOCK`]x[`BLOCK`] block of A, search the decimated plane for
//!    the best-matching block of B: an exhaustive coarse scan of the whole
//!    [`SEARCH_RADIUS_PX`] at an 8-pixel stride (SAD with early exit) followed
//!    by a diamond refinement down to a 2-pixel step. The coarse stage is what
//!    makes a large pan work on detailed content; a diamond alone starts on
//!    uncorrelated samples and never finds the true peak. The vector is a whole
//!    number of source pixels.
//! 3. Median-filter the vector field with a 3x3 window to remove isolated bad
//!    vectors, then re-measure each block's SAD at the *applied* vector. That
//!    SAD becomes the block's confidence.
//! 4. Warp A forward by `t*u` and B backward by `(1-t)*u`, blend them with the
//!    phase weights, and fade towards a plain cross-fade where confidence is
//!    low. A block whose best match is poor is exactly the place a warp would
//!    tear, so it degrades to the blend the user already knows.
//!
//! Everything runs in buffers owned by [`MotionInterp`] plus the caller's
//! output buffer: no pixel buffer is allocated per pair. The four passes (luma,
//! search, median/confidence, warp) split rows or blocks across at most
//! [`MAX_THREADS`] scoped worker threads; no crate, no thread pool of our own.
//!
//! Cost is budgeted, not assumed: [`MotionInterp::should_use`] refuses the
//! motion path while the last measured pair would not fit the caller's frame
//! slot, and backs off exponentially until a probe shows it fits again. The
//! caller then just blends that slot, so the pacer never pays for a pair it
//! cannot afford.

use std::time::Instant;

/// Side of a motion block, in source pixels.
pub const BLOCK: usize = 16;

/// Luma decimation for the search: one luma sample per 2x2 source pixels.
const DECIMATE: usize = 2;

/// Decimated block side (also the stride of one SAD window).
const HALF_BLOCK: usize = BLOCK / DECIMATE;

/// Diamond-search radius, in source pixels. 16 px covers normal panning; a
/// larger radius costs linearly and starts inventing matches in flat areas.
const SEARCH_RADIUS_PX: i32 = 16;

/// Largest frame this path accepts. 1280x720 is the measured budget (see
/// [`MAX_PIXELS`]); above it the caller clamps to plain blend.
pub const MAX_PIXELS: u64 = 1280 * 720;

/// Mean SAD per decimated sample at which a block counts as unmatched.
///
/// Two independent luma samples differ by ~85 on average; after the 2x2 box
/// average that drops to ~41. 14 leaves a wide margin over real matches (a
/// static or correctly warped block measures 0..3) and stays clearly below
/// noise, which is what makes the confidence fallback reliable.
const SAD_ZERO_CONFIDENCE: i32 = 14;

/// Hard cap on SAD evaluations per block, so one pathological search cannot
/// dominate the pair.
const MAX_CANDIDATES: u32 = 64;

/// Worker threads over row/block bands. Four bands is where memory bandwidth
/// stops scaling in the warp pass on this machine.
const MAX_THREADS: usize = 4;

/// Opportunities skipped after a pair overran its budget (doubles per repeat,
/// capped by [`MAX_SKIP`]). The last skipped opportunity is a probe.
const OVER_BUDGET_SKIPS: u32 = 4;
/// A probe every 64 opportunities (about 2 s at 30 pairs/s) is cheap enough that
/// a genuinely slow machine can stay safely in blend and still recover quickly
/// when the load drops.
const MAX_SKIP: u32 = 64;

/// The first pairs of a session are cold: first video frames, cold pixel
/// caches, freshly created worker stacks. They still teach the estimate (via
/// their best time) but never trigger the backoff, which would otherwise
/// throttle a mode that is in fact fast in steady state.
const WARMUP_PAIRS: u64 = 4;

/// A whole-pixel motion vector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Vec2 {
    x: i16,
    y: i16,
}

/// Reusable state for one decode session.
pub struct MotionInterp {
    w: usize,
    h: usize,
    /// Block grid: `bw * bh` vectors.
    bw: usize,
    bh: usize,
    /// Decimated luma planes.
    dw: usize,
    dh: usize,
    luma_a: Vec<u8>,
    luma_b: Vec<u8>,
    /// Search output, before the median filter.
    raw: Vec<Vec2>,
    /// Filtered field that the warp applies.
    field: Vec<Vec2>,
    /// Per-block confidence, `0..=256` (256 = a perfect match).
    conf: Vec<u16>,
    threads: usize,
    /// Measured cost of the last pair, ms.
    last_ms: f32,
    /// Smoothed pair cost, ms (the best of the cold-start warm-up pairs).
    ema_ms: f32,
    /// Backoff state: `skip_left` opportunities still to skip.
    backoff: u32,
    skip_left: u32,
    pairs: u64,
    /// Blocks whose confidence is zero in the last pair (QA/quality tests).
    low_conf: usize,
}

impl MotionInterp {
    pub fn new() -> Self {
        Self {
            w: 0,
            h: 0,
            bw: 0,
            bh: 0,
            dw: 0,
            dh: 0,
            luma_a: Vec::new(),
            luma_b: Vec::new(),
            raw: Vec::new(),
            field: Vec::new(),
            conf: Vec::new(),
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .clamp(1, MAX_THREADS),
            last_ms: 0.0,
            ema_ms: 0.0,
            backoff: 0,
            skip_left: 0,
            pairs: 0,
            low_conf: 0,
        }
    }

    /// Whether this frame size may use the motion path at all.
    ///
    /// The cap is a measured budget, not a preference: 720p needs ~5 ms of a
    /// pair on this machine, and 1080p more than doubles the pixel count and
    /// the search/warp cost with it, so anything above 1280x720 blends instead.
    pub fn supports(w: u32, h: u32) -> bool {
        w as usize >= BLOCK
            && h as usize >= BLOCK
            && (w as u64) * (h as u64) <= MAX_PIXELS
    }

    /// Allocate (or re-allocate) the working set for `w`x`h`. Buffers are only
    /// touched when the size actually changes, so calling this per source frame
    /// is free.
    pub fn reserve(&mut self, w: u32, h: u32) {
        if Self::supports(w, h) {
            self.prepare(w as usize, h as usize);
        }
    }

    /// Whether to attempt the next pair inside `budget_ms` of wall time.
    ///
    /// The first attempt of a session always runs, then the warm-up pairs teach
    /// the estimate. After that, an estimate above the budget skips a growing
    /// number of opportunities (4, 8, 16, ... up to [`MAX_SKIP`]) and probes
    /// once at the end of each backoff, so a transient stall cannot disable
    /// motion for the rest of the session.
    pub fn should_use(&mut self, budget_ms: f32) -> bool {
        let budget = if budget_ms.is_finite() {
            budget_ms.max(0.5)
        } else {
            30.0
        };
        if self.skip_left > 0 {
            self.skip_left -= 1;
            return self.skip_left == 0;
        }
        // The warm-up pairs must actually run to be measured; their individual
        // costs are cold and are not representative.
        if self.pairs < WARMUP_PAIRS {
            return true;
        }
        if self.ema_ms <= budget {
            return true;
        }
        // The estimate says a pair would overrun this slot: blend and schedule
        // a probe rather than trying (and overrunning) every interval.
        self.backoff = self
            .backoff
            .saturating_mul(2)
            .max(OVER_BUDGET_SKIPS)
            .min(MAX_SKIP);
        self.skip_left = self.backoff;
        false
    }

    /// Cost of the last completed pair, ms.
    pub fn last_pair_ms(&self) -> f32 {
        self.last_ms
    }

    /// Fraction of blocks that fell back to a plain cross-fade in the last
    /// pair. Zero for a static or perfectly matched pair; near 1 for noise.
    #[cfg(test)]
    fn low_confidence_ratio(&self) -> f32 {
        if self.bw * self.bh == 0 {
            return 0.0;
        }
        self.low_conf as f32 / (self.bw * self.bh) as f32
    }

    /// Interpolate one display frame between `a` (phase 0) and `b` (phase 1)
    /// into `out`, and record the pair's cost against `budget_ms`.
    ///
    /// Returns `false` without touching `out` when the inputs are unusable
    /// (unsupported size, short buffer); the caller then does a plain blend.
    /// `out` must not alias `a` or `b` (the borrow checker enforces that).
    pub fn interpolate(
        &mut self,
        out: &mut [u8],
        a: &[u8],
        b: &[u8],
        w: u32,
        h: u32,
        phase: f32,
        budget_ms: f32,
    ) -> bool {
        if !Self::supports(w, h) {
            return false;
        }
        self.interpolate_impl(out, a, b, w, h, phase, budget_ms)
    }

    /// QA measurement entry point: [`Self::interpolate_impl`] without the size
    /// gate, so a spike can price a 1080p pair instead of only observing that
    /// the product path refuses one. Test-only: the gate above is the product
    /// behaviour.
    #[cfg(test)]
    pub fn interpolate_beyond_cap(
        &mut self,
        out: &mut [u8],
        a: &[u8],
        b: &[u8],
        w: u32,
        h: u32,
        phase: f32,
        budget_ms: f32,
    ) -> bool {
        self.interpolate_impl(out, a, b, w, h, phase, budget_ms)
    }

    /// The four passes for one pair, plus the cost record. The caller has
    /// already decided that this frame size may run at all.
    fn interpolate_impl(
        &mut self,
        out: &mut [u8],
        a: &[u8],
        b: &[u8],
        w: u32,
        h: u32,
        phase: f32,
        budget_ms: f32,
    ) -> bool {
        let (wu, hu) = (w as usize, h as usize);
        let bytes = wu.saturating_mul(hu).saturating_mul(4);
        if out.len() < bytes || a.len() < bytes || b.len() < bytes || wu < BLOCK || hu < BLOCK {
            return false;
        }
        let started = Instant::now();
        // Every 32nd pair reports its pass split. Four timers per 32 pairs, and
        // the line explains a live slowdown (luma/search/warp) instead of
        // leaving only the total.
        let measure = self.pairs % 32 == 0;
        let mut split = [0f32; 4];
        let mut at = started;
        let mut lap = |slot: usize, at: &mut Instant| {
            if measure {
                let now = Instant::now();
                split[slot] = now.duration_since(*at).as_secs_f32() * 1000.0;
                *at = now;
            }
        };
        self.prepare(wu, hu);
        self.build_luma(a, b);
        lap(0, &mut at);
        self.search();
        lap(1, &mut at);
        self.refine();
        lap(2, &mut at);
        let phase = if phase.is_finite() {
            phase.clamp(0.0, 1.0)
        } else {
            0.5
        };
        self.warp(out, a, b, phase);
        lap(3, &mut at);
        if measure {
            crate::debug_log(&format!(
                "blockmatch split luma={:.3} search={:.3} refine={:.3} warp={:.3} frame={}x{} threads={}",
                split[0], split[1], split[2], split[3], w, h, self.threads
            ));
        }
        self.record(started.elapsed().as_secs_f64() * 1000.0, budget_ms);
        true
    }

    fn prepare(&mut self, w: usize, h: usize) {
        if self.w == w && self.h == h {
            return;
        }
        self.w = w;
        self.h = h;
        self.bw = (w + BLOCK - 1) / BLOCK;
        self.bh = (h + BLOCK - 1) / BLOCK;
        self.dw = (w + DECIMATE - 1) / DECIMATE;
        self.dh = (h + DECIMATE - 1) / DECIMATE;
        let blocks = self.bw * self.bh;
        self.luma_a = vec![0u8; self.dw * self.dh];
        self.luma_b = vec![0u8; self.dw * self.dh];
        self.raw = vec![Vec2::default(); blocks];
        self.field = vec![Vec2::default(); blocks];
        self.conf = vec![0u16; blocks];
        self.low_conf = 0;
        self.last_ms = 0.0;
        self.ema_ms = 0.0;
        self.backoff = 0;
        self.skip_left = 0;
        self.pairs = 0;
    }

    fn record(&mut self, ms: f64, budget_ms: f32) {
        let ms = ms as f32;
        self.last_ms = ms;
        self.pairs += 1;
        let budget = if budget_ms.is_finite() {
            budget_ms.max(0.5)
        } else {
            30.0
        };
        if self.pairs <= WARMUP_PAIRS {
            // Cold-start pairs: keep the best one as the estimate and never let
            // a cold spike set the backoff (see `WARMUP_PAIRS`).
            self.ema_ms = if self.pairs == 1 {
                ms
            } else {
                self.ema_ms.min(ms)
            };
            self.backoff = 0;
            self.skip_left = 0;
            return;
        }
        // A smoothed estimate, so one slow pass does not hide a fast machine
        // (and one fast pass does not hide a slow one).
        self.ema_ms = self.ema_ms * 0.7 + ms * 0.3;
        if ms > budget {
            // Never remember a pass as affordable; schedule the next attempt a
            // few opportunities later, doubling if the probes keep overrunning.
            if self.skip_left == 0 {
                self.backoff = self
                    .backoff
                    .saturating_mul(2)
                    .max(OVER_BUDGET_SKIPS)
                    .min(MAX_SKIP);
            }
            self.skip_left = self.skip_left.max(self.backoff);
        } else {
            self.backoff = 0;
            self.skip_left = 0;
        }
    }

    /// Decimated luma planes for both frames, using a 2x2 box average so a
    /// single bright pixel cannot fake a motion vector.
    ///
    /// This is a separate row-banded pass: both planes must be complete before
    /// any block can search. It is the largest compute stage of a pair
    /// (~1.2 ms at 720p on four threads), so it gets the same worker split.
    fn build_luma(&mut self, a: &[u8], b: &[u8]) {
        let (w, h, dw, dh) = (self.w, self.h, self.dw, self.dh);
        let threads = self.threads.min(dh.max(1));
        let band = (dh + threads - 1) / threads;
        let luma_a = &mut self.luma_a;
        let luma_b = &mut self.luma_b;
        std::thread::scope(|scope| {
            for (index, (rows_a, rows_b)) in luma_a
                .chunks_mut(band * dw)
                .zip(luma_b.chunks_mut(band * dw))
                .enumerate()
            {
                let first_row = index * band;
                scope.spawn(move || {
                    for (local, (row_a, row_b)) in
                        rows_a.chunks_mut(dw).zip(rows_b.chunks_mut(dw)).enumerate()
                    {
                        let dy = first_row + local;
                        if dy >= dh {
                            break;
                        }
                        let y0 = dy * DECIMATE;
                        let y1 = (y0 + DECIMATE - 1).min(h - 1);
                        for dx in 0..dw {
                            let x0 = dx * DECIMATE;
                            let x1 = (x0 + DECIMATE - 1).min(w - 1);
                            let i00 = (y0 * w + x0) * 4;
                            let i10 = (y0 * w + x1) * 4;
                            let i01 = (y1 * w + x0) * 4;
                            let i11 = (y1 * w + x1) * 4;
                            row_a[dx] = luma_box(a, i00, i10, i01, i11);
                            row_b[dx] = luma_box(b, i00, i10, i01, i11);
                        }
                    }
                });
            }
        });
    }

    /// One diamond search per block, over block-row bands.
    fn search(&mut self) {
        let (bw, bh, dw, dh) = (self.bw, self.bh, self.dw, self.dh);
        let threads = self.threads.min(bh.max(1));
        let band = (bh + threads - 1) / threads;
        let luma_a: &[u8] = &self.luma_a;
        let luma_b: &[u8] = &self.luma_b;
        let raw = &mut self.raw;
        std::thread::scope(|scope| {
            for (index, chunk) in raw.chunks_mut(band * bw).enumerate() {
                let first_row = index * band;
                scope.spawn(move || {
                    for (local, slots) in chunk.chunks_mut(bw).enumerate() {
                        let by = first_row + local;
                        for (bx, slot) in slots.iter_mut().enumerate() {
                            *slot = match_block(luma_a, luma_b, dw, dh, bx, by);
                        }
                    }
                });
            }
        });
    }

    /// 3x3 median filter, then confidence at the filtered vector.
    ///
    /// Each block reads its neighbours' raw vectors and writes only its own
    /// slot, so the pass splits over block rows exactly like the search.
    fn refine(&mut self) {
        let (bw, bh, dw, dh) = (self.bw, self.bh, self.dw, self.dh);
        let threads = self.threads.min(bh.max(1));
        let band = (bh + threads - 1) / threads;
        let raw: &[Vec2] = &self.raw;
        let luma_a: &[u8] = &self.luma_a;
        let luma_b: &[u8] = &self.luma_b;
        let field = &mut self.field;
        let conf = &mut self.conf;
        let low = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(threads);
            for (index, (vec_rows, conf_rows)) in field
                .chunks_mut(band * bw)
                .zip(conf.chunks_mut(band * bw))
                .enumerate()
            {
                let first_row = index * band;
                handles.push(scope.spawn(move || {
                    let mut low = 0usize;
                    for (local, (vecs, confs)) in
                        vec_rows.chunks_mut(bw).zip(conf_rows.chunks_mut(bw)).enumerate()
                    {
                        let by = first_row + local;
                        if by >= bh {
                            break;
                        }
                        let y0 = by * HALF_BLOCK;
                        let y1 = (y0 + HALF_BLOCK).min(dh);
                        for bx in 0..bw {
                            let x0 = bx * HALF_BLOCK;
                            let x1 = (x0 + HALF_BLOCK).min(dw);
                            let mut v = neighbourhood_median(raw, bw, bh, bx, by);
                            // A median of neighbours can point outside what this
                            // block can sample (it sits against the frame edge):
                            // clamp it, because the applied vector is the one
                            // confidence must describe. The field stores source
                            // pixels; the search and its limits are decimated.
                            let (min_dx, max_dx) =
                                offset_limits(x0, x1, dw, SEARCH_RADIUS_PX / DECIMATE as i32);
                            let (min_dy, max_dy) =
                                offset_limits(y0, y1, dh, SEARCH_RADIUS_PX / DECIMATE as i32);
                            let vx = (v.x as i32 / DECIMATE as i32).clamp(min_dx, max_dx);
                            let vy = (v.y as i32 / DECIMATE as i32).clamp(min_dy, max_dy);
                            v = Vec2 {
                                x: (vx * DECIMATE as i32) as i16,
                                y: (vy * DECIMATE as i32) as i16,
                            };
                            vecs[bx] = v;
                            let (sx, sy) =
                                ((x0 as i32 + vx) as usize, (y0 as i32 + vy) as usize);
                            let sad = sad_block(
                                luma_a,
                                luma_b,
                                dw,
                                x0,
                                y0,
                                sx,
                                sy,
                                x1 - x0,
                                y1 - y0,
                                i32::MAX,
                            );
                            let samples = ((x1 - x0) * (y1 - y0)).max(1) as i32;
                            let threshold = (SAD_ZERO_CONFIDENCE * samples).max(1);
                            let c = if sad >= threshold {
                                0
                            } else {
                                (((threshold - sad) * 256) / threshold) as u16
                            };
                            confs[bx] = c;
                            if c == 0 {
                                low += 1;
                            }
                        }
                    }
                    low
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or(0))
                .sum::<usize>()
        });
        self.low_conf = low;
    }

    /// Warp and blend pass, over pixel-row bands.
    fn warp(&mut self, out: &mut [u8], a: &[u8], b: &[u8], phase: f32) {
        let (w, h, bw, bh) = (self.w, self.h, self.bw, self.bh);
        let row_bytes = w * 4;
        let frame_bytes = h * row_bytes;
        // Only the frame itself is written; a pooled buffer that is longer than
        // one frame must not turn into extra rows that read past `a`/`b`.
        if out.len() < frame_bytes {
            return;
        }
        let out = &mut out[..frame_bytes];
        let tw = (phase * 256.0).round().clamp(0.0, 256.0) as u32;
        let wa = 256 - tw;
        // Weight of the residual plain cross-fade, in 1/256.
        let threads = self.threads.min(h.max(1));
        let band = (h + threads - 1) / threads;
        let wmax = (w - 1) as i32;
        let hmax = (h - 1) as i32;
        let field: &[Vec2] = &self.field;
        let conf: &[u16] = &self.conf;
        let (xa, xb) = (1.0 - phase, phase);
        std::thread::scope(|scope| {
            for (index, chunk) in out.chunks_mut(band * row_bytes).enumerate() {
                let y_start = index * band;
                let rows = chunk.len() / row_bytes;
                scope.spawn(move || {
                    // SAFETY: every offset below is clamped into the frame
                    // before use; `chunk` covers `rows` whole rows of `out`,
                    // `a` and `b` are whole frames of the same size, and the
                    // three buffers never overlap (`out` is exclusively
                    // borrowed by this call).
                    let out_ptr = chunk.as_mut_ptr();
                    let a_ptr = a.as_ptr();
                    let b_ptr = b.as_ptr();
                    for row in 0..rows {
                        let y = y_start + row;
                        let by = (y / BLOCK).min(bh - 1);
                        let out_row = unsafe { out_ptr.add(row * row_bytes) };
                        for bx in 0..bw {
                            let v = field[by * bw + bx];
                            let cw = conf[by * bw + bx] as u32;
                            // Forward warp of A, backward warp of B.
                            let dxa = ((v.x as f32) * xa).round() as i32;
                            let dya = ((v.y as f32) * xa).round() as i32;
                            let dxb = ((v.x as f32) * xb).round() as i32;
                            let dyb = ((v.y as f32) * xb).round() as i32;
                            let ay = ((y as i32) - dya).clamp(0, hmax) as usize;
                            let byy = ((y as i32) + dyb).clamp(0, hmax) as usize;
                            let row_a = ay * w;
                            let row_b = byy * w;
                            let row_p = y * w;
                            let x_end = ((bx + 1) * BLOCK).min(w);
                            if cw == 0 {
                                // Unmatched block: the plain cross-fade, which
                                // is exactly what the two extra samples would
                                // contribute at zero confidence.
                                for x in bx * BLOCK..x_end {
                                    let p = (row_p + x) * 4;
                                    let o = unsafe { out_row.add(x * 4) };
                                    for channel in 0..3 {
                                        // SAFETY: `p` is inside both frames.
                                        let sum = unsafe {
                                            *a_ptr.add(p + channel) as u32 * wa
                                                + *b_ptr.add(p + channel) as u32 * tw
                                        };
                                        unsafe {
                                            *o.add(channel) = ((sum + 128) >> 8) as u8;
                                        }
                                    }
                                    unsafe { *o.add(3) = 255 };
                                }
                            } else if cw == 256 {
                                // Fully matched block (the common case): the
                                // confidence must not damp the warp, so only the
                                // two warped samples are read. Halves the loads.
                                for x in bx * BLOCK..x_end {
                                    let ax = ((x as i32) - dxa).clamp(0, wmax) as usize;
                                    let bxx = ((x as i32) + dxb).clamp(0, wmax) as usize;
                                    let pa = (row_a + ax) * 4;
                                    let pb = (row_b + bxx) * 4;
                                    let o = unsafe { out_row.add(x * 4) };
                                    for channel in 0..3 {
                                        // SAFETY: both indices are clamped.
                                        let sum = unsafe {
                                            *a_ptr.add(pa + channel) as u32 * wa
                                                + *b_ptr.add(pb + channel) as u32 * tw
                                        };
                                        unsafe {
                                            *o.add(channel) = ((sum + 128) >> 8) as u8;
                                        }
                                    }
                                    unsafe { *o.add(3) = 255 };
                                }
                            } else {
                                // Partial confidence: fade between the warp and
                                // the cross-fade with the block's SAD weight. The
                                // four fixed-point weights sum to 65536.
                                let w_ac = wa * (256 - cw);
                                let w_bc = tw * (256 - cw);
                                let w_am = wa * cw;
                                let w_bm = tw * cw;
                                for x in bx * BLOCK..x_end {
                                    let ax = ((x as i32) - dxa).clamp(0, wmax) as usize;
                                    let bxx = ((x as i32) + dxb).clamp(0, wmax) as usize;
                                    let p = (row_p + x) * 4;
                                    let pa = (row_a + ax) * 4;
                                    let pb = (row_b + bxx) * 4;
                                    let o = unsafe { out_row.add(x * 4) };
                                    for channel in 0..3 {
                                        // SAFETY: all four indices are clamped.
                                        let sum = unsafe {
                                            *a_ptr.add(p + channel) as u32 * w_ac
                                                + *b_ptr.add(p + channel) as u32 * w_bc
                                                + *a_ptr.add(pa + channel) as u32 * w_am
                                                + *b_ptr.add(pb + channel) as u32 * w_bm
                                        };
                                        unsafe {
                                            *o.add(channel) = ((sum + 32768) >> 16) as u8;
                                        }
                                    }
                                    unsafe { *o.add(3) = 255 };
                                }
                            }
                        }
                    }
                });
            }
        });
    }
}

/// Decimated luma of the four corners of one 2x2 source cell.
#[inline(always)]
fn luma_box(src: &[u8], i00: usize, i10: usize, i01: usize, i11: usize) -> u8 {
    let r = (src[i00] as u32 + src[i10] as u32 + src[i01] as u32 + src[i11] as u32 + 2) >> 2;
    let g =
        (src[i00 + 1] as u32 + src[i10 + 1] as u32 + src[i01 + 1] as u32 + src[i11 + 1] as u32 + 2)
            >> 2;
    let b =
        (src[i00 + 2] as u32 + src[i10 + 2] as u32 + src[i01 + 2] as u32 + src[i11 + 2] as u32 + 2)
            >> 2;
    ((77 * r + 150 * g + 29 * b + 128) >> 8) as u8
}

/// SAD of two `rw`x`rh` windows, aborting once `limit` is exceeded.
#[inline(always)]
fn sad_block(
    a: &[u8],
    b: &[u8],
    stride: usize,
    ax: usize,
    ay: usize,
    bx: usize,
    by: usize,
    rw: usize,
    rh: usize,
    limit: i32,
) -> i32 {
    let mut sum = 0i32;
    for row in 0..rh {
        let mut i = (ay + row) * stride + ax;
        let mut j = (by + row) * stride + bx;
        let end = i + rw;
        while i < end {
            sum += (a[i] as i32 - b[j] as i32).abs();
            if sum > limit {
                return sum;
            }
            i += 1;
            j += 1;
        }
    }
    sum
}

/// Offsets a block starting at `start`/ending at `end` may use on an axis of
/// `size` samples without sampling outside it.
fn offset_limits(start: usize, end: usize, size: usize, radius: i32) -> (i32, i32) {
    let low = (-(start as i32)).max(-radius);
    let high = ((size as i32) - (end as i32)).min(radius);
    (low, high)
}

/// Diamond search (SAD, decimated luma) for one block. Returns the vector in
/// source pixels.
fn match_block(a: &[u8], b: &[u8], dw: usize, dh: usize, bx: usize, by: usize) -> Vec2 {
    let x0 = bx * HALF_BLOCK;
    let y0 = by * HALF_BLOCK;
    let x1 = (x0 + HALF_BLOCK).min(dw);
    let y1 = (y0 + HALF_BLOCK).min(dh);
    let (rw, rh) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if rw == 0 || rh == 0 {
        return Vec2::default();
    }
    let radius = SEARCH_RADIUS_PX / DECIMATE as i32;
    let (min_dx, max_dx) = offset_limits(x0, x1, dw, radius);
    let (min_dy, max_dy) = offset_limits(y0, y1, dh, radius);
    if min_dx > max_dx || min_dy > max_dy {
        return Vec2::default();
    }
    let mut best = sad_block(a, b, dw, x0, y0, x0, y0, rw, rh, i32::MAX);
    let (mut vx, mut vy) = (0i32, 0i32);
    let mut evaluated = 1u32;
    // Coarse exhaustive scan at a 4-sample stride, i.e. every 8 source pixels.
    //
    // A plain diamond cannot start at 16 px on detailed content: its first step
    // lands on uncorrelated samples, so the "best" neighbour is noise and the
    // true minimum is never reached (a 16 px pan then falls back to a blend for
    // most blocks). Sampling the whole radius coarsely first puts the search on
    // the right peak; the diamond below only has to refine by a few pixels.
    let coarse = (radius / 2).max(1) * 2;
    let mut cy = min_dy;
    while cy <= max_dy {
        let mut cx = min_dx;
        while cx <= max_dx {
            if (cx, cy) != (0, 0) {
                if evaluated >= MAX_CANDIDATES {
                    return Vec2 {
                        x: (vx * DECIMATE as i32) as i16,
                        y: (vy * DECIMATE as i32) as i16,
                    };
                }
                evaluated += 1;
                let sad = sad_block(
                    a,
                    b,
                    dw,
                    x0,
                    y0,
                    (x0 as i32 + cx) as usize,
                    (y0 as i32 + cy) as usize,
                    rw,
                    rh,
                    best,
                );
                if sad < best {
                    best = sad;
                    vx = cx;
                    vy = cy;
                }
            }
            cx += coarse;
        }
        cy += coarse;
    }
    // Local diamond refinement at steps 2 then 1, so a vector that is one or
    // two decimated samples off the coarse grid still lands on the true match.
    let mut step = 2i32.min(radius).max(1);
    loop {
        let mut candidate_best = best;
        let (mut cx, mut cy) = (vx, vy);
        let mut improved = false;
        for (dx, dy) in [(step, 0), (-step, 0), (0, step), (0, -step)] {
            let nx = vx + dx;
            let ny = vy + dy;
            if nx < min_dx || nx > max_dx || ny < min_dy || ny > max_dy {
                continue;
            }
            if evaluated >= MAX_CANDIDATES {
                return Vec2 {
                    x: (vx * DECIMATE as i32) as i16,
                    y: (vy * DECIMATE as i32) as i16,
                };
            }
            evaluated += 1;
            let sad = sad_block(
                a,
                b,
                dw,
                x0,
                y0,
                (x0 as i32 + nx) as usize,
                (y0 as i32 + ny) as usize,
                rw,
                rh,
                candidate_best,
            );
            if sad < candidate_best {
                candidate_best = sad;
                cx = nx;
                cy = ny;
                improved = true;
            }
        }
        if improved {
            vx = cx;
            vy = cy;
            best = candidate_best;
        } else if step == 1 {
            break;
        } else {
            step /= 2;
        }
    }
    Vec2 {
        x: (vx * DECIMATE as i32) as i16,
        y: (vy * DECIMATE as i32) as i16,
    }
}

/// Component-wise 3x3 median of the raw field around `(bx, by)`.
fn neighbourhood_median(raw: &[Vec2], bw: usize, bh: usize, bx: usize, by: usize) -> Vec2 {
    let mut xs = [0i16; 9];
    let mut ys = [0i16; 9];
    let mut n = 0usize;
    for ny in by.saturating_sub(1)..=(by + 1).min(bh - 1) {
        for nx in bx.saturating_sub(1)..=(bx + 1).min(bw - 1) {
            let v = raw[ny * bw + nx];
            xs[n] = v.x;
            ys[n] = v.y;
            n += 1;
        }
    }
    xs[..n].sort_unstable();
    ys[..n].sort_unstable();
    Vec2 {
        x: xs[n / 2],
        y: ys[n / 2],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 1280;
    const H: u32 = 720;

    fn hash(x: usize, y: usize, seed: u32) -> u32 {
        let mut n = (x as u32)
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add((y as u32).wrapping_mul(0x85EB_CA6B))
            .wrapping_add(seed.wrapping_mul(0xC2B2_AE35));
        n ^= n >> 13;
        n = n.wrapping_mul(0x27D4_EB2F);
        n ^ (n >> 16)
    }

    /// A pseudo-random but spatially coherent texture: per-pixel detail plus
    /// 16-pixel "blobs", so the search has both fine edges and large features.
    fn texture(w: u32, h: u32, seed: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let fine = hash(x, y, seed);
                let blob = hash(x / 16, y / 16, seed ^ 0x5EED);
                let i = (y * w as usize + x) * 4;
                out[i] = ((fine & 0x7F) + (blob & 0x7F)) as u8;
                out[i + 1] = (((fine >> 8) & 0x7F) + ((blob >> 8) & 0x7F)) as u8;
                out[i + 2] = (((fine >> 16) & 0x7F) + ((blob >> 16) & 0x7F)) as u8;
                out[i + 3] = 255;
            }
        }
        out
    }

    /// `dst(x, y) = src(x - shift, y)`, with edge pixels replicated.
    fn shift(src: &[u8], w: u32, h: u32, shift: i32) -> Vec<u8> {
        let mut out = vec![0u8; src.len()];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let sx = ((x as i32 - shift).clamp(0, w as i32 - 1)) as usize;
                let si = (y * w as usize + sx) * 4;
                let di = (y * w as usize + x) * 4;
                out[di..di + 4].copy_from_slice(&src[si..si + 4]);
            }
        }
        out
    }

    fn crossfade(out: &mut [u8], a: &[u8], b: &[u8]) {
        for i in 0..out.len() {
            out[i] = ((a[i] as u32 + b[i] as u32 + 1) >> 1) as u8;
        }
        for pixel in out.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }

    fn mae(a: &[u8], b: &[u8]) -> f64 {
        let mut sum = 0u64;
        for (x, y) in a.iter().zip(b.iter()) {
            sum += (*x as i32 - *y as i32).unsigned_abs() as u64;
        }
        sum as f64 / a.len().max(1) as f64
    }

    #[test]
    fn midpoint_beats_both_inputs_and_the_crossfade() {
        let a = texture(W, H, 1);
        let b = shift(&a, W, H, 16);
        let truth = shift(&a, W, H, 8);
        let mut out = vec![0u8; a.len()];
        let mut motion = MotionInterp::new();
        assert!(motion.interpolate(&mut out, &a, &b, W, H, 0.5, 30.0));
        let motion_err = mae(&out, &truth);
        let mut blend = vec![0u8; a.len()];
        crossfade(&mut blend, &a, &b);
        let blend_err = mae(&blend, &truth);
        let a_err = mae(&a, &truth);
        let b_err = mae(&b, &truth);
        crate::debug_log(&format!(
            "blockmatch quality shifted16 mae_motion={motion_err:.3} mae_blend={blend_err:.3} mae_a={a_err:.3} mae_b={b_err:.3} low_conf={:.3} pair_ms={:.3}",
            motion.low_confidence_ratio(),
            motion.last_pair_ms()
        ));
        assert!(
            motion_err < a_err * 0.5,
            "midpoint MAE {motion_err:.3} is not clearly better than input A ({a_err:.3})"
        );
        assert!(
            motion_err < b_err * 0.5,
            "midpoint MAE {motion_err:.3} is not clearly better than input B ({b_err:.3})"
        );
        assert!(
            blend_err > motion_err * 3.0,
            "cross-fade MAE {blend_err:.3} is not clearly worse than the motion midpoint {motion_err:.3}"
        );
        assert!(
            motion.low_confidence_ratio() < 0.1,
            "too many blocks fell back to the cross-fade: {:.3}",
            motion.low_confidence_ratio()
        );
    }

    #[test]
    fn a_static_pair_comes_back_near_identical() {
        let a = texture(W, H, 7);
        let mut out = vec![0u8; a.len()];
        let mut motion = MotionInterp::new();
        assert!(motion.interpolate(&mut out, &a, &a, W, H, 0.5, 30.0));
        let err = mae(&out, &a);
        crate::debug_log(&format!("blockmatch static mae={err:.4}"));
        assert!(err < 0.05, "static pair came back with MAE {err:.4}");
        assert_eq!(motion.low_confidence_ratio(), 0.0);
    }

    #[test]
    fn unmatched_noise_falls_back_to_the_crossfade() {
        let a = texture(W, H, 11);
        let b = texture(W, H, 12);
        let mut out = vec![0u8; a.len()];
        let mut motion = MotionInterp::new();
        assert!(motion.interpolate(&mut out, &a, &b, W, H, 0.5, 30.0));
        let mut blend = vec![0u8; a.len()];
        crossfade(&mut blend, &a, &b);
        let difference = mae(&out, &blend);
        crate::debug_log(&format!(
            "blockmatch noise mae_vs_blend={difference:.4} low_conf={:.3}",
            motion.low_confidence_ratio()
        ));
        assert!(
            motion.low_confidence_ratio() > 0.8,
            "noise was matched confidently: low_conf={:.3}",
            motion.low_confidence_ratio()
        );
        assert!(
            difference < 1.0,
            "noise fallback differs from the cross-fade by {difference:.3}"
        );
    }

    #[test]
    fn the_median_filter_removes_an_isolated_vector() {
        // 5x5 field, everything static except one outlier in the middle.
        let (bw, bh) = (5usize, 5usize);
        let mut raw = vec![Vec2::default(); bw * bh];
        raw[2 * bw + 2] = Vec2 { x: 32, y: -16 };
        raw[0] = Vec2 { x: 2, y: 2 };
        for by in 0..bh {
            for bx in 0..bw {
                let v = neighbourhood_median(&raw, bw, bh, bx, by);
                if (bx, by) != (0, 0) {
                    assert_eq!(v, Vec2::default(), "outlier survived at ({bx},{by})");
                }
            }
        }
        // The corner is its own 2x2 neighbourhood; the median is one of them.
        assert_eq!(neighbourhood_median(&raw, bw, bh, 0, 0).x, 0);
    }

    #[test]
    fn the_size_cap_matches_the_measured_budget() {
        assert!(MotionInterp::supports(W, H));
        assert!(MotionInterp::supports(960, 540));
        assert!(!MotionInterp::supports(1920, 1080));
        assert!(!MotionInterp::supports(1280, 722));
        assert!(!MotionInterp::supports(1282, 720));
        assert!(!MotionInterp::supports(8, 8));
        assert!(!MotionInterp::supports(0, 0));
    }

    #[test]
    fn a_slow_pair_backs_off_and_probes_again() {
        let mut motion = MotionInterp::new();
        // The first attempt of a session always runs: there is no measurement.
        assert!(motion.should_use(8.0));

        // Cold-start pairs teach the estimate (the best of them) and never
        // trigger the backoff, so a cold first pair cannot throttle the mode.
        motion.record(25.0, 8.0);
        assert_eq!(motion.backoff, 0);
        assert_eq!(motion.skip_left, 0);
        assert!((motion.ema_ms - 25.0).abs() < 1.0e-6);
        motion.record(6.0, 8.0);
        assert!((motion.ema_ms - 6.0).abs() < 1.0e-6);
        motion.record(20.0, 8.0);
        assert!((motion.ema_ms - 6.0).abs() < 1.0e-6);
        motion.record(7.0, 8.0);
        assert_eq!(motion.pairs, WARMUP_PAIRS);
        assert_eq!(motion.backoff, 0);

        // A measured over-budget pair after the warm-up starts the backoff,
        // which doubles while pairs keep overrunning and is capped.
        motion.record(25.0, 8.0);
        assert_eq!(motion.backoff, 4);
        assert_eq!(motion.skip_left, 4);
        assert!(motion.ema_ms > 8.0);
        // Four opportunities are skipped, the last one is the probe.
        assert!(!motion.should_use(8.0));
        assert!(!motion.should_use(8.0));
        assert!(!motion.should_use(8.0));
        assert!(motion.should_use(8.0));
        motion.record(25.0, 8.0);
        assert_eq!(motion.backoff, 8);
        motion.backoff = MAX_SKIP;
        motion.record(25.0, 8.0);
        assert_eq!(motion.backoff, MAX_SKIP);
        // A pair that fits again clears the backoff; the smoothed estimate
        // needs a few more fast pairs to fall back under the budget.
        motion.record(4.0, 8.0);
        assert_eq!(motion.backoff, 0);
        assert_eq!(motion.skip_left, 0);
        for _ in 0..4 {
            motion.record(4.0, 8.0);
        }
        assert!(motion.should_use(8.0));

        // A finite estimate above the budget also schedules a probe instead of
        // refusing the motion path forever.
        motion.ema_ms = 12.0;
        motion.backoff = 0;
        motion.skip_left = 0;
        assert!(!motion.should_use(8.0));
        assert_eq!(motion.skip_left, 4);
        assert!(!motion.should_use(8.0));
        assert!(!motion.should_use(8.0));
        assert!(!motion.should_use(8.0));
        assert!(motion.should_use(8.0));
    }

    #[test]
    fn pair_cost_is_within_the_720p_target() {        let a = texture(W, H, 3);
        let b = shift(&a, W, H, 16);
        let mut out = vec![0u8; a.len()];
        let mut motion = MotionInterp::new();
        // Warm up the allocator, the pages and the CPU caches.
        for _ in 0..3 {
            assert!(motion.interpolate(&mut out, &a, &b, W, H, 0.5, 30.0));
        }
        let mut times = Vec::with_capacity(12);
        for _ in 0..12 {
            let started = Instant::now();
            assert!(motion.interpolate(&mut out, &a, &b, W, H, 0.5, 30.0));
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = times[times.len() / 2];
        let best = times[0];
        let p95 = times[times.len() - 1];
        crate::debug_log(&format!(
            "blockmatch cost 720p ms_best={best:.3} ms_p50={p50:.3} ms_max={p95:.3} pairs_per_s_p50={:.1}",
            1000.0 / p50.max(1.0e-9)
        ));
        // Pass split (one warm pair): which stage owns the budget.
        motion.prepare(W as usize, H as usize);
        let mark = Instant::now();
        motion.build_luma(&a, &b);
        let luma_ms = mark.elapsed().as_secs_f64() * 1000.0;
        let mark = Instant::now();
        motion.search();
        let search_ms = mark.elapsed().as_secs_f64() * 1000.0;
        let mark = Instant::now();
        motion.refine();
        let refine_ms = mark.elapsed().as_secs_f64() * 1000.0;
        let mark = Instant::now();
        motion.warp(&mut out, &a, &b, 0.5);
        let warp_ms = mark.elapsed().as_secs_f64() * 1000.0;
        crate::debug_log(&format!(
            "blockmatch pass_ms luma={luma_ms:.3} search={search_ms:.3} refine={refine_ms:.3} warp={warp_ms:.3} threads={}",
            motion.threads
        ));
        // The 30 ms per-pair target (>= 30 pairs/s) is a release measurement;
        // an unoptimised test build is an order of magnitude slower by design.
        #[cfg(not(debug_assertions))]
        assert!(
            best < 30.0,
            "720p pair best {best:.3} ms misses the 30 ms (30 pairs/s) target"
        );
        #[cfg(debug_assertions)]
        assert!(best < 1000.0, "720p pair best {best:.3} ms is absurdly slow");
    }
}
