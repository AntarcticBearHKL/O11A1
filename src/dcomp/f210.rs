//! f210: the one trace that can tell "the pixels got expensive" from
//! "the driver got involved".
//!
//! # Why
//!
//! f209 shipped a picture that really runs at 120 Hz and a comment layer that
//! collapses from ~8 ms to ~180 ms per frame at t=19 s and never comes back
//! (`docs/f209_1080p_30s.log`). Two very different families of cause produce
//! that shape and the old per-second line cannot separate them:
//!
//! * a **device / swapchain** event - the layers re-created a swapchain, the
//!   driver evicted or lost the device, video memory ran out at the same moment
//!   the decoder started its second ring;
//! * a **cost** event - the same pixels, the same rows and the same damage rect
//!   simply became twenty times more expensive to produce on the CPU.
//!
//! So this module prints, once a second and only under `BILI_F210_TRACE=1`:
//!
//! * the `ID3D11Device` / immediate-context pointer of each of the three layers
//!   (UI, 弹幕, video) - equal pointers here are the "shared context" bug;
//! * per layer per second: `Map` count+bytes, `UpdateSubresource` count+bytes,
//!   `CopyResource` count, clears, draws, presents;
//! * the last `Present` HRESULT and `GetDeviceRemovedReason` of each layer;
//! * `IDXGIAdapter3::QueryVideoMemoryInfo(LOCAL)`: budget / usage /
//!   reservation, once a second;
//! * the swapchain size, format and **creation serial** of each layer - a
//!   resize or a rebuild moves the serial, which is what a t=19 s event would
//!   look like;
//! * the 弹幕 layer's own frame shape: rows, characters, damage rect, whether
//!   it was a full repaint, render/stage split, and `us_per_char` - the number
//!   that separates "more pixels" from "each glyph costs 20x".
//!
//! Everything is atomics on the presenting threads plus one sampler thread, so
//! the trace cannot change what it measures. `enabled()` is cached once: a run
//! without the variable pays one `OnceLock` read per present.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::Duration;

use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter3;

/// The three composition layers, in the order the trace prints them.
pub(crate) const UI: usize = 0;
pub(crate) const DANMAKU: usize = 1;
pub(crate) const VIDEO: usize = 2;
const ROLES: usize = 3;

struct Slot {
    device: AtomicU64,
    context: AtomicU64,
    /// Bumped by [`note_layer`] and [`note_resize`]: any change is a rebuild.
    swap_serial: AtomicU64,
    swap_w: AtomicU32,
    swap_h: AtomicU32,
    maps: AtomicU64,
    map_bytes: AtomicU64,
    updates: AtomicU64,
    update_bytes: AtomicU64,
    copies: AtomicU64,
    clears: AtomicU64,
    draws: AtomicU64,
    presents: AtomicU64,
    /// Last `Present` HRESULT (`S_OK` = 0, `DXGI_STATUS_OCCLUDED`, ...).
    present_hr: AtomicI64,
    /// Last `GetDeviceRemovedReason`; 0 = no reason, i.e. the device is fine.
    removed: AtomicI64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            device: AtomicU64::new(0),
            context: AtomicU64::new(0),
            swap_serial: AtomicU64::new(0),
            swap_w: AtomicU32::new(0),
            swap_h: AtomicU32::new(0),
            maps: AtomicU64::new(0),
            map_bytes: AtomicU64::new(0),
            updates: AtomicU64::new(0),
            update_bytes: AtomicU64::new(0),
            copies: AtomicU64::new(0),
            clears: AtomicU64::new(0),
            draws: AtomicU64::new(0),
            presents: AtomicU64::new(0),
            present_hr: AtomicI64::new(0),
            removed: AtomicI64::new(0),
        }
    }
}

static SLOTS: [Slot; ROLES] = [Slot::new(), Slot::new(), Slot::new()];

/// 弹幕 layer only: what one painted frame contained.
static DM_ROWS: AtomicU64 = AtomicU64::new(0);
static DM_CHARS: AtomicU64 = AtomicU64::new(0);
/// Distinct characters in the model: the number that decides whether the
/// software renderer's weighted glyph cache (1 MB of coverage in Slint 1.18)
/// can hold a whole frame or has to evict on every glyph.
static DM_UNIQ: AtomicU64 = AtomicU64::new(0);
static DM_DAMAGE_W: AtomicU64 = AtomicU64::new(0);
static DM_DAMAGE_H: AtomicU64 = AtomicU64::new(0);
/// 1 when the frame was a full-buffer render (first frame, resize, re-attach).
static DM_FULL: AtomicU64 = AtomicU64::new(0);
/// The two halves of `dm_paint_ms`, centi-milliseconds like the QA counters.
static DM_RENDER_CMS: AtomicU64 = AtomicU64::new(0);
static DM_STAGE_CMS: AtomicU64 = AtomicU64::new(0);
/// 1 when the strips path painted the frame, 0 for the Slint component.
static DM_STRIPS: AtomicU64 = AtomicU64::new(0);

/// Whether the trace is armed (`BILI_F210_TRACE=1`).
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("BILI_F210_TRACE")
            .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
    })
}

/// The adapter the memory query runs on. `IDXGIAdapter3` is free-threaded
/// (DXGI objects are), but the Rust binding is not `Send`, so it is wrapped.
struct SendAdapter(IDXGIAdapter3);

// SAFETY: DXGI objects are agile COM objects (`IDXGIAdapter3` documentation,
// "the DXGI adapter object is free-threaded"), and the only call made from the
// sampler thread is `QueryVideoMemoryInfo`, which reads a counter-like value.
unsafe impl Send for SendAdapter {}

static ADAPTER: Mutex<Option<SendAdapter>> = Mutex::new(None);

/// Register the adapter once (`Gpu::new`) and start the sampler.
pub(crate) fn register_adapter(factory: &windows::Win32::Graphics::Dxgi::IDXGIFactory2) {
    if !enabled() {
        return;
    }
    // Adapter 0 of the selected list is not necessarily the one the device was
    // created on, but `Gpu::new` prefers the desktop adapter with the most
    // memory, which is the one every layer's device uses; the memory numbers
    // are read for awareness, not for the per-layer attribution.
    if let Ok(adapter) = unsafe { factory.EnumAdapters(0) } {
        if let Ok(adapter3) = adapter.cast::<IDXGIAdapter3>() {
            if let Ok(mut slot) = ADAPTER.lock() {
                if slot.is_none() {
                    *slot = Some(SendAdapter(adapter3));
                }
            }
        }
    }
    spawn();
}

/// One `Layer` is born (or rebuilt at a new size).
pub(crate) fn note_layer(
    role: usize,
    device: usize,
    context: usize,
    width: u32,
    height: u32,
) {
    if !enabled() {
        return;
    }
    let Some(slot) = SLOTS.get(role) else {
        return;
    };
    slot.device.store(device as u64, Ordering::Relaxed);
    slot.context.store(context as u64, Ordering::Relaxed);
    slot.swap_w.store(width, Ordering::Relaxed);
    slot.swap_h.store(height, Ordering::Relaxed);
    slot.swap_serial.fetch_add(1, Ordering::Relaxed);
}

/// A swapchain was resized or rebuilt in place.
pub(crate) fn note_resize(role: usize, width: u32, height: u32) {
    if !enabled() {
        return;
    }
    let Some(slot) = SLOTS.get(role) else {
        return;
    };
    slot.swap_w.store(width, Ordering::Relaxed);
    slot.swap_h.store(height, Ordering::Relaxed);
    slot.swap_serial.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn bump(counter: &AtomicU64, bytes: &AtomicU64, count: u64, amount: u64) {
    counter.fetch_add(count, Ordering::Relaxed);
    bytes.fetch_add(amount, Ordering::Relaxed);
}

/// One `Map` + the bytes the CPU moved inside it.
pub(crate) fn note_map(role: usize, bytes: u64) {
    if !enabled() {
        return;
    }
    if let Some(slot) = SLOTS.get(role) {
        bump(&slot.maps, &slot.map_bytes, 1, bytes);
    }
}

/// One `UpdateSubresource` and its byte count.
pub(crate) fn note_update(role: usize, bytes: u64) {
    if !enabled() {
        return;
    }
    if let Some(slot) = SLOTS.get(role) {
        bump(&slot.updates, &slot.update_bytes, 1, bytes);
    }
}

pub(crate) fn note_copy(role: usize, count: u64) {
    if !enabled() {
        return;
    }
    if let Some(slot) = SLOTS.get(role) {
        slot.copies.fetch_add(count, Ordering::Relaxed);
    }
}

pub(crate) fn note_clear(role: usize) {
    if !enabled() {
        return;
    }
    if let Some(slot) = SLOTS.get(role) {
        slot.clears.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn note_draw(role: usize) {
    if !enabled() {
        return;
    }
    if let Some(slot) = SLOTS.get(role) {
        slot.draws.fetch_add(1, Ordering::Relaxed);
    }
}

/// One `Present` and its HRESULT.
pub(crate) fn note_present(role: usize, hr: i32) {
    if !enabled() {
        return;
    }
    if let Some(slot) = SLOTS.get(role) {
        slot.presents.fetch_add(1, Ordering::Relaxed);
        slot.present_hr.store(hr as i64, Ordering::Relaxed);
    }
}

/// The result of `GetDeviceRemovedReason` on this layer's device.
pub(crate) fn note_removed(role: usize, hr: i32) {
    if !enabled() {
        return;
    }
    if let Some(slot) = SLOTS.get(role) {
        slot.removed.store(hr as i64, Ordering::Relaxed);
    }
}

/// What one 弹幕 frame cost and contained.
pub(crate) struct DanmakuFrame {
    pub(crate) rows: usize,
    pub(crate) chars: usize,
    pub(crate) uniq: usize,
    pub(crate) damage: (u32, u32),
    pub(crate) full: bool,
    pub(crate) render_ms: f32,
    pub(crate) stage_ms: f32,
    pub(crate) strips: bool,
}

pub(crate) fn note_danmaku(frame: &DanmakuFrame) {
    if !enabled() {
        return;
    }
    DM_ROWS.store(frame.rows as u64, Ordering::Relaxed);
    DM_CHARS.store(frame.chars as u64, Ordering::Relaxed);
    DM_UNIQ.store(frame.uniq as u64, Ordering::Relaxed);
    DM_DAMAGE_W.store(frame.damage.0 as u64, Ordering::Relaxed);
    DM_DAMAGE_H.store(frame.damage.1 as u64, Ordering::Relaxed);
    DM_FULL.store(u64::from(frame.full), Ordering::Relaxed);
    DM_RENDER_CMS.store((frame.render_ms * 100.0) as u64, Ordering::Relaxed);
    DM_STAGE_CMS.store((frame.stage_ms * 100.0) as u64, Ordering::Relaxed);
    DM_STRIPS.store(u64::from(frame.strips), Ordering::Relaxed);
}

fn spawn() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::Builder::new()
            .name("f210-trace".into())
            .spawn(run)
            .ok();
    });
}

fn run() {
    let mut last = [(0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64); ROLES];
    let mut second = 0u64;
    // The first line one second in: the layers have presented by then, and a
    // t=19 s transition needs a series, not a burst.
    std::thread::sleep(Duration::from_millis(1000));
    loop {
        second += 1;
        let mut parts = vec![format!("f210 trace t={second}s")];
        let (mut dm_ctx, mut video_ctx) = (0u64, 0u64);
        let mut line_roles = Vec::with_capacity(ROLES);
        for (role, name) in ["ui", "dm", "video"].into_iter().enumerate() {
            let slot = &SLOTS[role];
            let device = slot.device.load(Ordering::Relaxed);
            let context = slot.context.load(Ordering::Relaxed);
            if role == DANMAKU {
                dm_ctx = context;
            }
            if role == VIDEO {
                video_ctx = context;
            }
            let maps = slot.maps.load(Ordering::Relaxed);
            let map_bytes = slot.map_bytes.load(Ordering::Relaxed);
            let updates = slot.updates.load(Ordering::Relaxed);
            let update_bytes = slot.update_bytes.load(Ordering::Relaxed);
            let copies = slot.copies.load(Ordering::Relaxed);
            let clears = slot.clears.load(Ordering::Relaxed);
            let draws = slot.draws.load(Ordering::Relaxed);
            let presents = slot.presents.load(Ordering::Relaxed);
            let (lm, lmb, lu, lub, lc, lcl, ld, lp) = last[role];
            last[role] = (maps, map_bytes, updates, update_bytes, copies, clears, draws, presents);
            line_roles.push(format!(
                "{name}={{dev=0x{device:x} ctx=0x{context:x} swap={}x{}#{} \
                 map={}/{map_delta_b} upd={}/{upd_delta_b} copy={copy_delta} \
                 clear={clear_delta} draw={draw_delta} present={present_delta} \
                 hr=0x{present_hr:x} removed=0x{removed:x}}}",
                slot.swap_w.load(Ordering::Relaxed),
                slot.swap_h.load(Ordering::Relaxed),
                slot.swap_serial.load(Ordering::Relaxed),
                maps.saturating_sub(lm),
                updates.saturating_sub(lu),
                copy_delta = copies.saturating_sub(lc),
                clear_delta = clears.saturating_sub(lcl),
                draw_delta = draws.saturating_sub(ld),
                present_delta = presents.saturating_sub(lp),
                present_hr = slot.present_hr.load(Ordering::Relaxed) as u32,
                removed = slot.removed.load(Ordering::Relaxed) as u32,
                map_delta_b = map_bytes.saturating_sub(lmb),
                upd_delta_b = update_bytes.saturating_sub(lub),
            ));
        }
        parts.push(format!(
            "ctx_shared={}",
            u8::from(dm_ctx == video_ctx && dm_ctx != 0)
        ));
        parts.extend(line_roles);
        // Video memory: the numbers a WDDM eviction would move.
        if let Ok(slot) = ADAPTER.lock() {
            if let Some(adapter) = slot.as_ref() {
                let mut info = windows::Win32::Graphics::Dxgi::DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                let ok = unsafe {
                    adapter
                        .0
                        .QueryVideoMemoryInfo(
                            0,
                            windows::Win32::Graphics::Dxgi::DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
                            &mut info,
                        )
                        .is_ok()
                };
                if ok {
                    const MB: f64 = 1024.0 * 1024.0;
                    parts.push(format!(
                        "vram_local_mb={:.0}/{:.0}/{:.0}",
                        info.Budget as f64 / MB,
                        info.CurrentUsage as f64 / MB,
                        info.CurrentReservation as f64 / MB
                    ));
                }
            }
        }
        let rows = DM_ROWS.load(Ordering::Relaxed);
        let chars = DM_CHARS.load(Ordering::Relaxed);
        let uniq = DM_UNIQ.load(Ordering::Relaxed);
        let render_ms = DM_RENDER_CMS.load(Ordering::Relaxed) as f32 / 100.0;
        let stage_ms = DM_STAGE_CMS.load(Ordering::Relaxed) as f32 / 100.0;
        // The decisive ratio: cost per visible character. A damage-area effect
        // moves both numbers together; a per-glyph effect leaves the area and
        // the rows alone and multiplies this one.
        let per_char = if chars > 0 {
            render_ms * 1000.0 / chars as f32
        } else {
            0.0
        };
        parts.push(format!(
            "dm_rows={rows} dm_chars={chars} dm_uniq_chars={uniq} dm_damage={}x{} \
             dm_full={} dm_render_ms={render_ms:.2} dm_stage_ms={stage_ms:.2} \
             dm_us_per_char={per_char:.1} dm_strips={}",
            DM_DAMAGE_W.load(Ordering::Relaxed),
            DM_DAMAGE_H.load(Ordering::Relaxed),
            DM_FULL.load(Ordering::Relaxed),
            DM_STRIPS.load(Ordering::Relaxed),
        ));
        crate::debug_log(&parts.join(" "));
        std::thread::sleep(Duration::from_millis(1000));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The role table is the whole point of the module: three distinct slots.
    #[test]
    fn roles_are_distinct_slots() {
        assert!(DANMAKU != UI && VIDEO != UI && VIDEO != DANMAKU);
        assert!(SLOTS.get(VIDEO).is_some());
        assert!(SLOTS.get(ROLES).is_none());
    }
}
