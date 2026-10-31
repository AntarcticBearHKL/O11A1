//! f192 side probe: what a *software-rendered* Slint UI layer costs per frame.
//!
//! The DirectComposition prototype (`examples/dcomp_proto.rs`) proves the two
//! swapchains. The other half of the plan is "render the player's `.slint` UI
//! into a premultiplied RGBA buffer and upload that as the UI layer", which
//! needs `slint::platform::software_renderer`. This binary renders a
//! representative player overlay (toolbar band, title, sliding counter,
//! translucent info panel, six danmaku lines) into a premultiplied buffer at
//! both 1280x720 and 1920x1080 and reports the per-frame cost.
//!
//! It is a separate process on purpose: `slint::platform::set_platform` is
//! process-global and can only be called once, which is exactly the constraint
//! that decides how the player window can be wired in.
//!
//! Run:
//!   cargo run --release --example slint_soft_probe

use std::rc::Rc;
use std::time::Instant;

use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize};

slint::slint! {
    export component Overlay inherits Window {
        in property <int> tick: 0;
        in property <length> shift: 0px;
        in property <string> label: "";
        in property <color> tint: transparent;
        default-font-family: "Noto Sans SC";
        background: transparent;
        // Full-window wash, behind everything: setting it every frame makes the
        // whole window dirty, which is the worst case for the software renderer.
        Rectangle {
            x: 0; y: 0;
            width: parent.width; height: parent.height;
            background: root.tint;
        }
        toolbar := Rectangle {
            x: 0; y: 0;
            width: parent.width; height: 72px;
            background: #0e1014cc;
        }
        Text {
            x: 24px; y: 18px;
            text: "O11A1 · 软件渲染 UI 层 · 弹幕与工具栏";
            color: #f2f4f8;
            font-size: 26px;
        }
        Text {
            x: parent.width - 460px; y: 20px;
            text: root.label;
            color: #cfd6e4;
            font-size: 24px;
        }
        Rectangle {
            x: parent.width - 300px; y: 120px;
            width: 270px; height: 420px;
            background: #12141acc;
            border-radius: 12px;
            border-width: 1px;
            border-color: #ffffff30;
            Text {
                x: 16px; y: 16px;
                text: "选集\n\n1  第 1 集\n2  第 2 集\n3  第 3 集\n4  第 4 集";
                color: #e6eaf2;
                font-size: 22px;
            }
        }
        for i in 6 : Text {
            x: 200px + root.shift + i * 24px;
            y: 220px + i * 48px;
            text: "这条弹幕位于视频之上 · danmaku line";
            color: #ffffff;
            font-size: 28px;
        }
        Rectangle {
            x: 0; y: parent.height - 120px;
            width: parent.width; height: 120px;
            background: #0b0d11d9;
            Text {
                x: 32px; y: 34px;
                text: "▶  ✦  01:23 / 05:40   ⋯";
                color: #eef1f7;
                font-size: 30px;
            }
        }
    }
}

struct SoftPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for SoftPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

fn pct(values: &mut [f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[((values.len() as f64 - 1.0) * fraction).round() as usize]
}

fn measure(
    ui: &Overlay,
    window: &Rc<MinimalSoftwareWindow>,
    width: u32,
    height: u32,
    frames: usize,
    full_repaint: bool,
) -> (f64, f64, f64) {
    ui.window().set_size(PhysicalSize::new(width, height));
    let mut buffer = vec![PremultipliedRgbaColor::default(); (width as usize) * (height as usize)];
    let mut times = Vec::with_capacity(frames);
    let mut drawn = 0usize;
    let start_all = Instant::now();
    for tick in 0..frames {
        ui.set_tick(tick as i32);
        ui.set_shift((tick % 120) as f32 * 2.0);
        if full_repaint {
            let wave = (tick % 60) as u8;
            ui.set_tint(slint::Color::from_argb_u8(16, wave, wave / 2, 255 - wave));
        }
        ui.set_label(format!("frame {tick} · 120 Hz").as_str().into());
        let started = Instant::now();
        let redrawn = window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, width as usize);
        });
        if redrawn {
            times.push(started.elapsed().as_secs_f64() * 1000.0);
            drawn += 1;
        }
    }
    let elapsed = start_all.elapsed().as_secs_f64();
    let mut times = times;
    (
        pct(&mut times, 0.5),
        pct(&mut times, 0.95),
        drawn as f64 / elapsed,
    )
}

/// Paint one whole frame into a fresh buffer and write it out. `NewBuffer` makes
/// the renderer repaint everything instead of only the dirty regions.
fn dump(ui: &Overlay, window: &Rc<MinimalSoftwareWindow>, width: u32, height: u32) {
    ui.window().set_size(PhysicalSize::new(width, height));
    ui.set_tick(4242);
    ui.set_shift(0.0);
    ui.set_label("frame 4242 · 120 Hz".into());
    ui.set_tint(slint::Color::from_argb_u8(0, 0, 0, 0));
    let mut buffer = vec![PremultipliedRgbaColor::default(); (width as usize) * (height as usize)];
    let mut painted = false;
    let _ = window.draw_if_needed(|renderer| {
        renderer.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
        renderer.render(&mut buffer, width as usize);
        painted = true;
    });
    let mut rgba = vec![0u8; buffer.len() * 4];
    let mut covered = 0usize;
    for (index, pixel) in buffer.iter().enumerate() {
        let alpha = pixel.alpha as u32;
        if alpha > 0 {
            covered += 1;
        }
        let unpremultiply = |value: u8| -> u8 {
            if alpha == 0 {
                0
            } else {
                ((value as u32 * 255 / alpha).min(255)) as u8
            }
        };
        rgba[index * 4] = unpremultiply(pixel.red);
        rgba[index * 4 + 1] = unpremultiply(pixel.green);
        rgba[index * 4 + 2] = unpremultiply(pixel.blue);
        rgba[index * 4 + 3] = pixel.alpha;
    }
    let path = format!("target/qa/f192_soft_{width}x{height}.png");
    let _ = std::fs::create_dir_all("target/qa");
    let _ = image::save_buffer(&path, &rgba, width, height, image::ExtendedColorType::Rgba8);
    println!(
        "soft: {width}x{height} full paint painted={painted} covered={:.1}% -> {path}",
        covered as f64 * 100.0 / buffer.len().max(1) as f64
    );
}

fn main() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    if let Err(error) = slint::platform::set_platform(Box::new(SoftPlatform {
        window: window.clone(),
    })) {
        println!("soft: platform FAIL {error}");
        return;
    }
    let ui = match Overlay::new() {
        Ok(ui) => ui,
        Err(error) => {
            println!("soft: component FAIL {error}");
            return;
        }
    };
    // The player's UI imports assets/fonts/O11A1Round-*.ttf inside app.slint,
    // which embeds them in the generated component. An inline `slint!` component
    // has no file context, so the same fonts are registered at runtime instead.
    for path in [
        "assets/fonts/O11A1Round-Regular.ttf",
        "assets/fonts/O11A1Round-Bold.ttf",
    ] {
        match window.renderer().register_font_from_path(std::path::Path::new(path)) {
            Ok(()) => println!("soft: registered {path}"),
            Err(error) => println!("soft: font {path} FAILED {error}"),
        }
    }
    if let Err(error) = ui.show() {
        println!("soft: show FAIL {error}");
        return;
    }
    println!(
        "soft: renderer=software platform=minimal channels=premultiplied alpha (buffer is PremultipliedRgbaColor)"
    );
    for (width, height) in [(1280u32, 720u32), (1920, 1080)] {
        dump(&ui, &window, width, height);
        for full in [false, true] {
            // Warm up the text layout caches, then measure.
            let _ = measure(&ui, &window, width, height, 30, full);
            let (p50, p95, per_second) = measure(&ui, &window, width, height, 240, full);
            let kind = if full { "full-window repaint" } else { "dirty region (danmaku moving)" };
            println!(
                "soft: {width}x{height} {kind}: frame p50={p50:.2} p95={p95:.2} ms sustains={per_second:.1} frames/s -> {}",
                if p50 <= 8.0 {
                    "fits a 120 Hz UI layer"
                } else if p50 <= 16.6 {
                    "60 Hz UI layer"
                } else {
                    "slower than 60 Hz"
                }
            );
        }
    }
}
