//! f196: the danmaku layer's QA capture hook.
//!
//! `BILI_F196_CAPTURE=<tag>[,<delay_ms>[,<W>x<H>]]` snapshots the player
//! window's own Slint tree into `BILI_CAPTURE_DIR/<tag>.png` after `delay_ms`,
//! optionally resizing the window to `WxH` first.
//!
//! Why a snapshot and not a desktop grab: the winit path draws the picture and
//! the comments in ONE window, and a snapshot carries exactly that tree
//! (`Renderer::take_snapshot`), so "the comments are inside the picture rect and
//! not on the letterbox" is provable without hunting the HWND from another
//! process (winit reports no window handle at session creation, which is what
//! made the f195 capture script fragile).
//!
//! The composite path has no tree for the video or the danmaku layer (they are
//! composition swapchains), so its evidence is the real scan-out
//! (`scripts/qa-f196.ps1`, `CopyFromScreen`).

use std::time::Duration;

use slint::ComponentHandle;

/// `BILI_CAPTURE_DIR` (default `target/qa`) + `<tag>.png`.
fn capture_path(tag: &str) -> std::path::PathBuf {
    let dir = crate::qa::env::var("BILI_CAPTURE_DIR").unwrap_or_else(|| "target/qa".into());
    std::path::Path::new(&dir).join(format!("{tag}.png"))
}

/// Parse and arm the hook. No-op without the variable.
pub(crate) fn f196_spawn_capture(window: &slint::Weak<crate::PlayerWindow>) {
    let Some(spec) = crate::qa::env::var("BILI_F196_CAPTURE") else {
        return;
    };
    let mut parts = spec.split(',');
    let tag = parts.next().unwrap_or("f196_capture").trim().to_string();
    let delay_ms = parts
        .next()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(2500);
    let size = parts.next().and_then(|raw| {
        let (w, h) = raw.trim().split_once('x')?;
        Some((w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?))
    });
    let weak = window.clone();
    std::thread::spawn(move || {
        crate::media_hooks::note_thread_role("f196-capture");
        std::thread::sleep(Duration::from_millis(delay_ms));
        let _ = slint::invoke_from_event_loop(move || {
            let Some(player) = weak.upgrade() else {
                crate::debug_log("f196 capture failed reason=no-window");
                return;
            };
            if let Some((w, h)) = size {
                player.window().set_size(slint::LogicalSize::new(w, h));
            }
            // One more event-loop turn so the resize's relayout has happened.
            let weak = player.as_weak();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(player) = weak.upgrade() else {
                    return;
                };
                let path = capture_path(&tag);
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let size = match player.window().take_snapshot() {
                    Ok(buffer) => {
                        let bytes: Vec<u8> = buffer
                            .as_slice()
                            .iter()
                            .flat_map(|pixel| [pixel.r, pixel.g, pixel.b, pixel.a])
                            .collect();
                        match image::RgbaImage::from_raw(
                            buffer.width(),
                            buffer.height(),
                            bytes,
                        ) {
                            Some(png) => {
                                let _ = png.save(&path);
                                format!("{}x{}", buffer.width(), buffer.height())
                            }
                            None => "None".into(),
                        }
                    }
                    Err(error) => format!("err={error}"),
                };
                crate::debug_log(&format!(
                    "f196 capture tag={tag} size={size} path={}",
                    path.display()
                ));
            });
        });
    });
}
