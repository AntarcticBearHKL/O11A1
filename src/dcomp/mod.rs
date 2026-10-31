//! f194: DirectComposition for the player window, winit for everything else.
//!
//! # Why a hybrid platform
//!
//! `slint::platform::set_platform` is process-global and one-shot. Before this
//! module existed the process never called it at all: Slint's default selector
//! picked `i-slint-backend-winit`, and every window (main, mini, player, tray
//! popup) went through that one backend.
//!
//! The player window has to stop going through it. Its picture band needs a
//! composition layer that runs at the pacer's rate (see
//! `examples/dcomp_proto.rs`: 117.6-119.97 fps at 1080p) with the Slint UI as a
//! second, alpha-blended layer above it - and an HWND that owns no redirection
//! bitmap is the only way to get per-pixel transparency out of a Slint window on
//! Windows.
//!
//! So the process installs [`platform::HybridPlatform`] once. It *is* the winit
//! backend (`i_slint_backend_winit::Backend` is held inside and every event
//! loop, clipboard and font-size call is forwarded to it), with one exception:
//! `create_window_adapter` looks at a thread-local flag set by
//! [`with_player_scope`]. When the flag is up, the adapter being built is the
//! player window; when it is down the request goes to winit unchanged, which is
//! every other window in the app.
//!
//! # The fallback
//!
//! f204: the branch is a user setting (`settings.json` → `player_dcomp`,
//! switchable from the player's 画质增强 menu); f212 made **on** the shipped
//! default. `BILI_PLAYER_DCOMP` still wins over it, and `=0` (or
//! `false`/`no`/`off`) turns it off for one run: the flag is still set,
//! `create_window_adapter` sees it, declines, and the player is built by winit
//! exactly as before. Nothing else in the process changes.

pub(crate) mod danmaku;
/// f202: 画质增强 — CAS / Anime4K / FSR in the sink's own passes.
pub(crate) mod enhance;
/// f210: the 1 Hz device / upload / video-memory trace (`BILI_F210_TRACE=1`).
pub(crate) mod f210;
pub(crate) mod gpu;
pub(crate) mod platform;
/// f209: the NV12 decoder surface on the GPU (video-processor YUV->RGB, the
/// zero-upload picture path).
pub(crate) mod nv12;
pub(crate) mod player_window;
/// f207: 弹幕条带 — the comment layer's text rasterised once, blitted every
/// frame (`danmaku.rs` keeps the model, the geometry and the fallback).
pub(crate) mod strips;
pub(crate) mod video;
pub(crate) mod warp;

/// Whether the warp pipeline prints each step (`BILI_WARP_TRACE=1`, or always
/// for the first job so a crash leaves a last line behind).
pub(crate) fn warp_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("BILI_WARP_TRACE").is_ok_and(|value| {
            matches!(value.trim(), "1" | "true" | "yes" | "on")
        })
    })
}

pub(crate) use platform::{install_platform, with_player_scope};
pub(crate) use player_window::{
    accepts_video, attached, danmaku_counters, danmaku_model, danmaku_stats, is_player_window,
    prime_video_layer, repaint, set_danmaku_visible, sink_for, submit_frame, sync_layout,
    ui_counters, ui_paints_total, video_counters, VideoLayout,
};
pub(crate) use video::{MfSource, MfState, VideoSink};
