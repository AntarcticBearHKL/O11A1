# native_player

A standalone, Slint-free native video player window for O11A1: Win32 +
DirectComposition + D3D11 + Direct2D/DirectWrite, with ffmpeg for decode and
WinMM `waveOut` for audio. The window is frameless and wears the old O11A1
player chrome: a custom 44px top strip, a floating 300px right sidebar
(信息/选集/评论/性能/设置), a compact centered toolbar, and the 弹幕 settings
popover, all drawn in the D2D overlay over the full-bleed video.

This is a migration building block. `src/lib.rs` exposes the API the future main
UI embeds; `src/main.rs` is a small CLI/env runner for the standalone
`native_player.exe`.

## Build / run

```powershell
$env:PATH = "C:\Users\antar\.cargo\bin;$env:PATH"
$env:CARGO_TARGET_DIR = "C:\Users\antar\AppData\Local\Temp\opencode\o11a1-player-target"
cargo build --release -p native_player
cargo test -p native_player

# default video (BV1mhxo6BEG4), windowed 1280x720
$env:BILI_OPEN_BVID = "BV1mhxo6BEG4"
& "$env:CARGO_TARGET_DIR\release\native_player.exe"
```

Open a direct URL or a local file:

```powershell
$env:BILI_PLAYER_URL = "D:\videos\clip.mp4"
.\native_player.exe
```

The QA screenshot recipe is `native_player\qa.ps1`; it launches the player,
keeps it topmost/foreground, waits for the danmaku list, 插帧 + 番剧 + the stats
panel, captures the composited window with GDI `CopyFromScreen` (there is no
redirection surface for `PrintWindow` on this window), and waits for the
auto-exit.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1
```

## Library API

```rust
use native_player::{AspectMode, EnhanceMode, InterpMode, Player, PlayerConfig};

let player = Player::open(PlayerConfig {
    bvid: Some("BV1mhxo6BEG4".into()),
    ..Default::default()
})?;
player.set_interp(InterpMode::Cpu);
player.set_enhance(EnhanceMode::Anime);
player.set_danmaku_visible(true);
println!("danmaku: {} comments, visible={}", player.danmaku_count(), player.danmaku_visible());
player.toggle_mini();
player.set_aspect(AspectMode::Fill);
player.toggle_always_on_top();
player.set_brightness(1.2);
player.toggle_audio_only();
player.screenshot();          // <root>/screenshots/O11A1_<timestamp>.png
player.seek(30_000);
player.set_volume(80);
player.pause();
player.play();
println!("{} / {}", player.position_ms(), player.duration_ms());
player.wait();       // until the window is closed or a QA auto-exit fires
# Ok::<(), anyhow::Error>(())
```

`Player::open` resolves the source and creates the window on a dedicated player
thread; commands go through a channel and `position_ms`/`duration_ms`/
`is_playing`/`fps` are lock-free reads of shared atomics, so a UI thread never
blocks on the video thread. `close()` (or `Drop`) kills both ffmpeg children and
joins every thread.

`PlayerConfig` also takes `cid`, `direct_url`, `title`, `start_ms`,
`quality_qn`, `quality_codecid` and `audio_index`. Environment overrides: `BILI_OPEN_BVID`, `BILI_PLAYER_URL`,
`BILI_PLAYER_VOLUME`, `BILI_PLAYER_SIZE`, `BILI_PLAYER_DANMAKU` (0/1 initial
danmaku visibility), `BILI_PLAYER_MINI=1` (start in the mini player). Danmaku
controls: `set_danmaku_visible(bool)`, `toggle_danmaku()`, `danmaku_visible()`,
`danmaku_count()`. Mini: `toggle_mini()`, `is_mini()`. Picture: `set_aspect`/
`cycle_aspect`/`aspect()` (`AspectMode::{Fit,Fill,Stretch}`), `set_always_on_top`/
`toggle_always_on_top`/`is_topmost()`, `set_brightness`/`brightness()`,
`set_audio_only`/`toggle_audio_only`/`is_audio_only()`, `screenshot()`, and
`speed_up()`/`speed_down()` (the same 0.5–2.0× grid as `[` / `]`).

### Source selection

* `BILI_PLAYER_URL` / `direct_url` wins: HTTP(S) or a local path.
* Otherwise `BILI_OPEN_BVID` / `bvid` (default `BV1mhxo6BEG4`) is resolved with
  `video_view` + `playurl`, using the saved account when one exists and an
  anonymous session otherwise.
* Video: requested qn 80, AVC preferred, falling back to `default_qn`; the
  picture is decoded at its native size, capped at 1920×1080 preserving aspect
  and even NV12 dimensions.
* Audio: highest-bitrate non-VIP DASH track (or the first track).
* 画质/音轨 start from the same `playurl` response. The 设置 tab opens the full
  stream menu: every playable `(qn, codecid)` row, every locked 大会员 tier
  (`需要大会员`), and every DASH audio index. A selection re-runs `playurl` on the
  shared session in a background thread, rebuilds the video swapchain only when
  the decoded size changes, reopens decoder + audio at the current position and
  preserves pause/play. `PlayerConfig::quality_codecid` / `audio_index` pin the
  initial stream.
* `Session::media_headers()` supplies UA/Referer/Cookie to ffmpeg/ffprobe.
* Danmaku: the session that resolved the source is handed to a background
  thread, which calls `Session::danmaku_all(cid, duration_secs)` and sends the
  converted list to the player thread. The video is never blocked by the fetch.

## Controls

| Key | Action |
|-----|--------|
| Space | 播放 / 暂停 |
| ← / → | 快退 / 快进 5 s |
| ↑ / ↓ | 音量 ±5 |
| M | 静音 |
| Ctrl+M / Alt+M | 迷你窗 开 / 关 |
| D | 弹幕 开 / 关 |
| F / F11 | 全屏 |
| S | 截图到 `<paths::root()>/screenshots/O11A1_<YYYYMMDD_HHMMSS>.png` |
| A | 比例：适应 / 填充 / 拉伸 |
| T | 置顶 开 / 关（迷你窗内始终保持置顶） |
| - / = (or Shift+↓/↑) | 亮度 −/+ 10%（范围 40–200%） |
| 0 | 亮度恢复 100% |
| V | 仅音频 开 / 关（视频黑场，弹幕保留） |
| [ / ] | 速度 0.5 / 0.75 / 1.0 / 1.25 / 1.5 / 2.0× |
| I | 插帧：关闭 / CPU |
| E | 画质增强：关闭 / 通用 / 番剧 |
| P | 性能 tab（统计） |
| Esc | 关闭弹幕设置 → 关闭侧栏 → 退出全屏 → 关闭窗口 |

These are the default bindings. The 11 player actions (播放/暂停, 快退, 快进,
音量+/-, 静音, 全屏, 退出全屏, 弹幕, 点赞, 截图) are read from
`settings.player_keys` at open; the 15-key pool is the old one (M/N/B/空格/
方向键/F/D/Q/S/Esc/P/回车). Remapping 播放 to F, for example, makes F play/pause
and leaves F11 as the legacy fullscreen key. I/E/A/T/V/-/=/[ ]/0 keep their
extra functions.

Mouse: click the video to play/pause, drag the thin progress line above the
toolbar to seek, wheel to change volume (or scroll the sidebar under the
pointer), double-click the video to toggle fullscreen. Drag the empty top strip
to move the window; double-click the top strip to maximize/restore. The toolbar
and top strip fade after 3 s without activity and return on the next mouse move;
the sidebar stays visible while it is open. In mini mode a click toggles
play/pause and a double-click restores the normal window.

## Player chrome

* **Frameless window.** `WS_POPUP | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX`
  with `WM_NCCALCSIZE` returning a full client and `WM_NCHITTEST` supplying the
  6px resize edges. DWM gets `DWMWCP_ROUND`; drag uses `WM_NCLBUTTONDOWN/HTCAPTION`.
  The custom strip is 44px: title left, qn/codec badge right, minimize /
  maximize / close buttons at the far right (buttons are inside the video width
  when the sidebar is open).
* **Right sidebar.** 300px overlay over the full-bleed video, tabs
  信息 / 选集 / 评论 / 历史 / 性能 / 设置 and a close X. 信息 shows the fetched cover,
  title, UP主, views/danmaku/date, 点赞/投币/收藏 actions and the description slot
  (the core `VideoDetail` exposes no description, so this build shows
  `暂无简介`). 选集 lists `video_view.pages` and clicking a row resolves that
  `cid` through playurl, reopens at 0 on the same quality, and refetches its
  danmaku (same decoded size only). 评论 uses `Session::comments(aid, hot, cursor)`
  with 热门/时间 sorting, circular avatars, two-line messages, likes, 加载更多
  and the end state. 性能 is the old stats table. 设置 opens interactive 画质
  and 音轨 menus (playable `(qn, codecid)` rows, locked 大会员 tiers with a clear
  message, every DASH audio index), speed, aspect, brightness, 插帧, 画质增强,
  弹幕, 弹幕设置, 置顶, 仅音频, 睡眠定时 (15/30/60/90 min cycling, pauses on
  expiry) and a read-only 快捷键 row showing what settings loaded. In light
  theme the sidebar is the old white panel while the top strip and toolbar stay
  dark; the theme comes from `settings.theme`.
* **Progress hover.** Hovering the seek line fetches `videoshot` once on a
  background worker, downloads/decodes the sprite sheets, computes the cell from
  `index`, and draws that cell plus a time bubble above the progress line. A
  fetch failure degrades to the time-only bubble. Dragging uses the same preview.
* **History heartbeat.** While playing, a background worker calls
  `Session::report_heartbeat` every 15 s with the real aid (from `video_view` on
  the detail worker, or lazily by the heartbeat worker), bvid/cid, played and
  watched seconds, duration, start timestamp and play_type 1/0/2/3/4
  (start/playing/pause/resume/end). Closing sends the end beat and joins the
  worker. `resume.json` stays the main UI's local resume store; this player does
  not write history or resume files synchronously.
* **Toolbar.** 640×54 pill centered over the video (it shifts left when the
  sidebar is open), with a thin draggable seek line above it. Buttons:
  play/pause, current/duration, volume/mute, 弹幕, 弹幕设置, speed, aspect,
  screenshot, mini, fullscreen, 选集, sidebar and 设置.
* **弹幕设置 popup.** Opacity, 字号 小/标准/大, speed, area 25/50/75/100%,
  block 滚动/顶部/底部, density and a read-only 发送 box. Changes apply live and
  are persisted to `settings.json`; sending is intentionally not wired.
* **Background metadata.** A `player-detail` thread shares the Session used for
  playback, fetches `video_view` once, then serves comments and like/coin/fav
  actions. None of this runs on the player thread, so the pacer is unaffected.

## Architecture

* One player thread owns the HWND, the message pump, the D3D11 device, both
  composition swapchains and the pacer.
* Window: `WS_EX_NOREDIRECTIONBITMAP` + `DCompositionCreateDevice` +
  `CreateTargetForHwnd`. The HWND is frameless (`WS_POPUP | WS_THICKFRAME`);
  `WM_NCCALCSIZE` gives the whole window to the client, `WM_NCHITTEST` supplies
  the resize rim, `WM_MOUSELEAVE` drives the fade, and DWM rounds the corners.
* Video layer: `CreateSwapChainForComposition` (`DXGI_ALPHA_MODE_IGNORE`,
  `FLIP_SEQUENTIAL`, 2 buffers, frame-latency waitable), BGRA staging upload,
  `Present(0, DXGI_PRESENT_DO_NOT_WAIT)`; only a real `S_OK` counts as a
  presented frame.
* UI layer: a second composition swapchain
  (`DXGI_ALPHA_MODE_PREMULTIPLIED`) drawn on demand with Direct2D/DirectWrite.
  It paints the top strip, sidebar tabs/content, compact toolbar, progress line,
  danmaku popover and toast. Bitmap thumbnails/avatars are cached by image id.
  The overlay redraws on state changes (and at 5 Hz for the clock), never once
  per video frame.
* Decode: two ffmpeg children. Video writes raw NV12 into a 6-frame bounded
  ring, paced at the track rate and gated while paused/seeking/starting. A full
  ring now backpressures the reader with a condvar instead of dropping the
  oldest frame; this matters because the audio-master clock can sit 100–300 ms
  behind wall time, and dropping that frame left `take_due` permanently chasing
  a moving queue (the `picture stalled` case). Seek/stop signals the condvar.
  NV12 → BGRA is SSE2 (8 pixels/iteration, BT.709 limited range).
* Audio: ffmpeg `s16le` 48 kHz stereo → four prepared 20 ms `waveOut` buffers.
  The pipe/channel backpressure bounds buffering to 160 ms. `waveOutPause` /
  `waveOutRestart` pause/resume, `waveOutSetVolume` controls volume.
* Clock: the audio device position is the master when the sound is ready;
  otherwise the wall clock advances the media timeline. A seek resets the audio
  `ready` flag, freezes the new first picture until the new sound session has
  data, then resumes both at the target. Drift over 200 ms logs
  `player av-drift`.
* 插帧 CPU: the last two decoded BGRA frames are blended (SSE2/AVX2,
  `(a*(256-w)+b*w+128)>>8`) on a `min(60, display_hz)` grid. A 30 fps source
  presents source/blend alternately at 60 fps; a 60 fps source is unchanged.
* 画质增强: the shipped `src/dcomp/enhance.rs` chain ported without Slint —
  AMD CAS (通用, strength 0.6, one pass) and Anime4K v4.0.1 Restore-CNN-S
  (番剧: Clamp_Highlights + 4 CNN passes, `RGBA16F` intermediates). Any shader,
  texture or pass failure logs `enhance fallback` and returns to 关闭; the plain
  copy still presents that frame.
* 弹幕: a **third composition swapchain** between the video and the control
  overlay (full client size, premultiplied alpha). The comments are D2D/DirectWrite
  text with a dark offset shadow, drawn on the display's own cadence (up to
  120 Hz, never tied to the 60 fps video grid) only while the video plays and
  comments are active; the control overlay keeps its 5 Hz/on-demand cadence.
  On this 120 Hz panel a dense run measured ~100–104 danmaku-layer presents per
  second at 1080p while video stayed at 30 fps; the 120 Hz ceiling is the panel
  and the remaining gap is player-thread CPU, not the old fixed 60 Hz cap.
  Scrolling comments use the video clock, precomputed lanes and a conservative
  width estimate; a lane is reused only after its right edge has fully entered
  the frame. Top/bottom comments last 4 s and use their own row occupancy.
* Background workers: `player-detail` (`video_view`, comments, actions),
  `player-heartbeat` (history), `player-storyboard` (videoshot sheets) and the
  per-switch playurl thread all share the session but never block the player
  thread. Decode EOF before the media tail, audio-device/session failure and a
  15 s first-frame timeout produce a centered error card with a 重试 button;
  retry stops both sessions and reopens at the saved position. A queue with no
  presented picture for 2 s shows the animated 正在缓冲… stall overlay.
* Fullscreen toggles the window style/rect; the UI swapchain is resized and the
  composition clip follows the client area.
* 比例 is a DirectComposition transform only; the decoded picture and swapchain
  stay at the source size. 适应 uses a uniform fit and draws black letterbox bars,
  填充 uses a uniform cover scale (the root clip crops the overflow), 拉伸 uses
  independent X/Y scales. The overlay only paints bars in 适应.
* 亮度 is a video-layer pixel-shader multiply (`brightness_main`), or a multiply
  in the final CAS/Anime4K pass, so the UI and danmaku layers are untouched.
  0.4–2.0 is clamped; 1.0 takes the plain copy fast path. Changing it while paused
  re-presents the held frame.
* 仅音频 clears the video swapchain backbuffer to black and keeps presenting at
  the normal cadence; audio, the pacer, and the danmaku layer keep running.
  Toggling back re-presents the held picture immediately when paused.
* 置顶 uses `SetWindowPos(HWND_TOPMOST/HWND_NOTOPMOST)` without moving or
  activating the window. Mini entry forces topmost; mini restore reapplies the
  user's pre-mini 置顶 state.
* `S` captures the composited window with the existing GDI `window::capture`
  (there is no redirection surface for `PrintWindow`) and never pauses playback.

### 迷你窗

* `Ctrl+M` / `Alt+M` (or `toggle_mini()`, or QA `mini`) shrinks the **client**
  to 480×270 (16:9), clamped to the monitor work area, centered, and sets
  `HWND_TOPMOST`. The frameless mini keeps its top 26 px as a drag handle.
* The full overlay is hidden: the mini UI layer draws only the letterbox bars
  and a 2 s “Ctrl+M 还原” hint in the top-right. The video and danmaku layers
  fill the client; danmaku lanes use the whole video band (no top bar offset).
* Restore returns to the exact saved outer rect and reapplies the 置顶 state the
  user had before mini; the normal overlay, stats and control bars come back.
  Clicking the mini video toggles play/pause; double-click restores. Entering
  mini from fullscreen exits fullscreen first.
* `BILI_PLAYER_MINI=1` starts mini. The frameless mini is exactly 480×270
  outer and client (there is no title bar).

### 弹幕 model and settings

* `DanmakuModel` is seeded from `progress_ms` and advanced with the same
  audio-master clock as the picture. Pause stops drawing/motion; resume continues
  from the same position. Seek clears active comments and re-seeds from the
  target (no old comment is replayed); end clears the layer.
* Settings come from `o11a1::settings::load()` at start: `danmaku_opacity`
  (0–100), `danmaku_font` (0/1/2 → 0.85/1.0/1.2), `danmaku_speed`
  (50–200, 100 = 8 s crossing), `danmaku_area` (25/50/75/100 % of the video
  band), `block_scroll`/`block_top`/`block_bottom`, `blocked_senders`,
  case-insensitive substring `blocked_keywords`, and `blocked_regex` (compiled
  with the `regex` crate already in the workspace lock; invalid patterns are
  ignored and counted in the settings log).
* Font hints 18/25/36 map to 22/27/32 px at 1080p and scale with the window
  height; at 720p they are ~15/18/21 px.
* The overlay shows `弹幕: 开/关` in the bottom chips and `弹幕 n（屏上 m）` in
  the stats panel. `D` toggles, `BILI_PLAYER_DANMAKU=0` starts off.

## QA hooks

`BILI_PLAYER_QA` is a comma list applied at `3.5 s + 1.8 s` per action:
`seek`, `pause`, `resume`/`play`, `interp`, `enhance`/`cas`, `anime`/`enhance2`,
`stats`, `mute`, `fullscreen`/`fs`, `danmaku`/`dm` (ensure on), `danmakuoff`/`dmoff`
(ensure off), `danmakutoggle`/`dmtoggle` (toggle), `mini`/`minitoggle` (toggle
the mini player), `aspect`/`ratio` (cycle 适应→填充→拉伸), `ontop`/`topmost`
(toggle), `bright`/`brightness` (100→60→150→200%), `audioonly`/`audio`
(toggle), `speedup`/`faster`, `speeddown`/`slower`, `shot`/`screenshot`,
`sidebar` (toggle), `info`, `episodes`/`parts`, `comments`, `performance`/`perf`,
`settings` (open that right-strip tab), `dmpopup`/`danmakupopup`, `chrome`
(show/hold the chrome), `comments_time`, `like`, `coin`, `fav`/`favorite`,
`page2` (switch to the second 分P when present), `quality`/`audio_menu` (open
the 设置 stream menu), `quality_low`/`quality_next` (select a row through the
same menu handler), `quality_locked` (exercise the 大会员 message), `audio_next`,
`comments_more` (pagination request), `hover`/`hover2` (pin a progress hover and
fetch the storyboard), `sleep` (cycle 15/30/60/90/off), `sleep_short` (3 s
expiry test), `fault` (QA-injected error card for the retry UI), `retry` and
`stall` (5 s injected buffering overlay). Danmaku interaction tokens are
`dm_menu`, `dm_hold`, `dm_like`, `dm_report`, `dm_report_one`, `dm_block`,
`dm_copy`, `dm_undo`, `dm_history`, `dm_history_date` and `dm_history_close`
(see the 2026-11-10 section below).
`BILI_PLAYER_SECONDS` auto-exits. The QA `shot` action keeps its legacy output
path: `BILI_PLAYER_SHOT`, or `%TEMP%\opencode\native_player_qa.png`. The `S` key
(and `Player::screenshot()`) writes the timestamped
`<o11a1::paths::root()>/screenshots/O11A1_<YYYYMMDD_HHMMSS>.png` instead.

Danmaku verification used the default upload plus a dense comment track:

```powershell
$env:BILI_OPEN_BVID = "BV1mhxo6BEG4"
$env:BILI_PLAYER_QA = "danmaku,stats"
$env:BILI_PLAYER_SECONDS = "12"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Qa "danmaku,stats" -Seconds 12

# 189 comments, ~15 on screen at once, 30 fps source with CPU interpolation
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mv4Y6rESW -Qa "danmaku,interp,stats" -Seconds 20 `
  -OutPng "$env:TEMP\opencode\native_player_danmaku_rich.png"
```

Observed: first frame at ~0.8–1.5 s, audio started ~1.0–1.3 s, 60.0 presented fps at
852×480 with 插帧 CPU and 画质增强 番剧, 0.4–0.7 ms per blend, seek to 30 s
landed and both clocks matched, pause/resume and fullscreen/mute/seek were
exercised without errors. On the danmaku pass: the required upload fetches 9
comments (one real scrolling comment is visible in `native_player_danmaku.png`),
the dense track fetches 189–188 comments and shows 15 on screen at 59.9 fps
(`native_player_danmaku_rich.png`). The 30 fps source keeps 60 fps with one
active comment; the dense 30 fps track keeps 59.9 fps with 15. Pause freezes the
danmaku layer (`dm_frames` unchanged) and resume continues; seek logs
`danmaku seek reset` and re-seeds at the target.

Mini verification:

```powershell
$env:BILI_OPEN_BVID = "BV1mhxo6BEG4"
$env:BILI_PLAYER_QA = "mini,stats"
$env:BILI_PLAYER_SECONDS = "16"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Qa "mini,stats" -Seconds 16 `
  -OutPng "$env:TEMP\opencode\native_player_mini.png"

# restore path
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Qa "mini,mini,stats" -Seconds 16 `
  -OutPng "$env:TEMP\opencode\native_player_mini_restore.png"
```

Observed: `mini=1 client=480x270 saved_client=1280x720 saved_rect=1296x759 topmost=1`,
then `mini=0 client=1280x720 restored=1280x720 topmost=0`. The mini screenshot is
a real 480×270-client video window with no full chrome; the restore screenshot is
the original 1280×720 window with the full overlay and stats. `BILI_PLAYER_MINI=1`
logs the same `mini=1` at startup. The screenshots are in
`C:\Users\antar\AppData\Local\Temp\opencode\`.

Player-side feature verification (all logs from the release build):

```powershell
# 比例: cycles 适应 → 填充, then stats
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "aspect,stats" -Seconds 14 `
  -OutPng "$env:TEMP\opencode\native_player_aspect_fill.png"

# 置顶: one toggle on, then on→off
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "ontop,ontop,stats" -Seconds 12 `
  -OutPng "$env:TEMP\opencode\native_player_ontop_off.png"

# 亮度: 100 → 60%
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "bright,stats" -Seconds 12 `
  -OutPng "$env:TEMP\opencode\native_player_bright2.png"

# 仅音频 on, then off
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "audioonly,audioonly,stats" -Seconds 10 `
  -OutPng "$env:TEMP\opencode\native_player_audioonly_off.png"

# 速度: 1.0 → 1.25×
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "speedup,stats" -Seconds 12 `
  -OutPng "$env:TEMP\opencode\native_player_speedup.png"
```

Observed logs: `aspect mode=1 label=填充 client=1280x720`,
`ontop=1 effective=1 mini=0` then `ontop=0 effective=0 mini=0`,
`brightness=0.60 percent=60`, `audio_only=1` then `audio_only=0`,
`speed=1.25`. Every run kept the 30 fps pacer (or 36–38 fps at 1.25×) and the
danmaku layer advancing. Brightness at 60% was also combined with `enhance`
(通用) and `anime` (番剧): both compiled, logged `enhance first-frame`, kept
30 fps, and showed 亮度 60% with no `enhance fallback`. After the ring
backpressure fix the required `aspect,stats` run logs
`queue=5..6 dropped=0` around every second, and a
separate `interp,stats` run presents 59–61 fps with `interp=CPU`, so the 60 fps
path and the 弹幕 cadence are intact. `cargo test -p native_player` is 19 tests
(16 original + 3 aspect-transform tests). A 900×800 window makes the crop/拉伸 visually obvious:
`native_player_aspect_fill_crop.png` and `native_player_aspect_stretch.png`.
With 置顶 on before mini, restore logged
`mini=1 ... topmost=1` → `mini=0 ... topmost=1`; with the default it logged
`topmost=0`. The real `S` key wrote
`...\release\screenshots\O11A1_20261110_121412.png` (1296×759, mean RGB
~81/80/88, so not black; the executable folder is `paths::root()` for this
build because its name is `o11a1-player-target`, not a component named
`target`) without pausing; the next log entries continued at 30 fps. A
synchronous PNG encode can briefly cost frames in that one-second window, but
the playback clock and audio never pause.

Player chrome verification (release build; screenshots in
`C:\Users\antar\AppData\Local\Temp\opencode\`):

```powershell
# windowed, sidebar open on 信息
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "info,stats" -Seconds 12 `
  -OutPng "$env:TEMP\opencode\final_chrome_info.png"

# 评论 (real first page, avatars, sort buttons)
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "comments,stats" -Seconds 14 `
  -OutPng "$env:TEMP\opencode\final_chrome_comments.png"

# 性能 / 设置 / 弹幕 popup
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "performance,stats" -Seconds 10 `
  -OutPng "$env:TEMP\opencode\new_chrome_perf.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "settings,stats" -Seconds 10 `
  -OutPng "$env:TEMP\opencode\new_chrome_settings.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "dmpopup,stats" -Seconds 10 `
  -OutPng "$env:TEMP\opencode\new_chrome_popup.png"

# fullscreen: no top strip, toolbar visible
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1mhxo6BEG4 -Qa "fullscreen" -Seconds 8 `
  -OutPng "$env:TEMP\opencode\final_chrome_fullscreen.png"

# multi-P: 5-page list and a real P1→P2 switch
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 `
  -Bvid BV1BBnb6aEww -Qa "episodes,page2,stats" -Seconds 16 `
  -OutPng "$env:TEMP\opencode\final_chrome_page2.png"
```

Observed: `detail info bvid=... pages=5 cover=1`; the sidebar logs
`sidebar=1 tab=信息/评论/选集`; comments log
`comments loaded n=4 total=25 end=1` (anonymous endpoint returns only the
pinned/first slice for this upload); page switch logs
`source switch request ... cid=43263459402 page=2` then
`source switched ...`, reopens at 0, and refetches `danmaku loaded n=127`.
The fullscreen capture is 1920×1080 with no top strip and the compact toolbar;
the windowed captures are 1280×720. `interp,stats` still presents 60 fps.
Automated clicks verified: toolbar 弹幕 toggle, toolbar sidebar toggle, sidebar
close X, maximize button, and double-click top-strip maximize/restore
(`zoomed=True` → `restored=True`). The idle watcher logged
`chrome hidden idle_ms=3000`; the QA harness's repeated foreground/topmost calls
can keep it visible in a screenshot.

## P0 player-gap verification (2026-11-10)

Commands used the shared target
`C:\Users\antar\AppData\Local\Temp\opencode\o11a1-parallel-player-target` and
`O11A1_HOME=C:\Users\antar\AppData\Local\Temp\opencode\player-qa-home`
(a copy of the saved account plus `theme=1`), so the screenshots are the light
sidebar. The QA executable is passed explicitly because `qa.ps1`'s default
`-Exe` points at the older target.

```powershell
$exe = "$env:TEMP\o11a1-parallel-player-target\release\native_player.exe"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "settings,quality,stats" -Seconds 22 `
  -OutPng "$env:TEMP\player_light_quality.png" -LogPath "$env:TEMP\player_light_quality.log"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "settings,audio_menu,stats" -Seconds 14 `
  -OutPng "$env:TEMP\player_light_audio.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "settings,quality_low,stats" -Seconds 16 `
  -OutPng "$env:TEMP\player_quality_menu_switch.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "settings,audio_next,stats" -Seconds 14 `
  -OutPng "$env:TEMP\player_audio_menu_switch.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "hover,stats" -Seconds 16 -OutPng "$env:TEMP\player_storyboard_hover.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "comments,stats" -Seconds 18 -OutPng "$env:TEMP\player_comments_light2.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "settings,sleep,stats" -Seconds 14 -OutPng "$env:TEMP\player_sleep_timer.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "settings,sleep_short,stats" -Seconds 16
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "fault,stats" -Seconds 10 -OutPng "$env:TEMP\player_error_retry.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Qa "fault,retry,stats" -Seconds 14
```

Observed (`BV1mhxo6BEG4`):

* Open: `source bilibili ... qn=80 codec=AVC size=1920x1080 -> 1920x1080 ...
  audio=30232/78699 audio_index=1 tracks=12 tiers=5`; the 1080P default is
  unchanged. `theme mode=1 light_sidebar=1`;
  `player keys source=settings actions=11 keys=空格/←/→/↑/↓/M…`.
* 画质 menu: `settings menu=Quality rows=13` (12 playable `(qn, codecid)` rows +
  the locked `1080P+` `需要大会员` row). `quality_low` logged
  `quality select qn=16 codecid=7 label=360P · AVC`, then
  `source track switch ... qn=16 codec=AVC ... size=640x360`, a new
  `video surface swapchain=640x360`, `source track applied ... position_ms=5244`,
  `seek landed target_ms=5244 gen=3 playing=1 holding=1`, and 30 fps afterwards.
  The screenshot shows the 360P picture and `画质 360P · AVC` in the sidebar.
* 音轨 menu: `settings menu=Audio rows=3`; `audio_next` logged
  `audio select index=2 label=64K (30216) · 65 kbps`, `source track switch ...
  audio_index=2`, `source track applied ... position_ms=4568`, `seek landed ...
  playing=1`, then 30 fps. Both switches preserve the current play state.
* History: `heartbeat start ...`, then
  `heartbeat sent play_type=1 aid=117560418767049 bvid=BV1mhxo6BEG4
  cid=43336207479 played_s=0 ... code=0`,
  `play_type=0 ... played_s=14 watched_s=15 ... code=0` at ~15 s, and
  `play_type=4 ... watched_s=21 ... code=0` on close. Pause/resume runs logged
  `play_type=2` and `play_type=3`. `heartbeat worker stopped` follows the end
  beat. The aid also came from `detail info ... aid=...`; the worker lazily
  fetched `video_view` when the start beat raced the detail worker.
* Storyboard: `storyboard sheet loaded 4800x2700 bytes=51840000`,
  `storyboard ready ... cols=10 rows=10 cell=480x270 tiles=84`,
  `storyboard cell index=29 time_ms=174650 sheet=0 col=9 row=2`; the hover
  screenshot shows the 480×270 cell and `02:54` above the progress line.
* Comments: `detail comments ... n=8 total=25 end=0`,
  `detail avatars loaded n=8`; the screenshot shows round clipped avatars,
  `热门/时间`, and 加载更多. `comments_more` logged
  `comments request ... reset=0 cursor=CAES…` then
  `detail comments ... n=0 ... end=1` (the next page of this upload is empty).
* Sleep: `settings,sleep` showed `睡眠定时 14:57` and logged
  `sleep timer set minutes=15`; `sleep_short` logged
  `sleep timer set seconds=3 qa=1`, then `sleep timer expired action=pause`,
  `pause at_ms=7659`, and a `play_type=2` heartbeat.
* Error: `fault` logged `player error text=QA 注入：视频解码中断（用于验证重试按钮）`
  and drew the centered 播放中断 card with the 重试 button. `fault,retry` logged
  `retry request target_ms=2611`, `retry sessions reopened`, `seek landed
  target_ms=2611 gen=3 playing=1`, and 30 fps resumed.
* Danmaku cadence: `danmaku cadence hz=120 independent=1`; on
  `BV1mv4Y6rESW` with 3207 comments and 50 active, `dm_frames` advanced
  ~104 per second while the video reported 30 fps, so the comment layer is no
  longer pinned to the video/60 Hz grid. The remaining ~16 Hz gap to 120 is
  player-thread CPU on this run, not a hard-coded cap.
* `cargo test -p native_player` is 28 unit tests + 1 doc-test, all green;
  `cargo check -p native_ui` is green (its pre-existing dead-code warnings
  remain).

## Danmaku interactions + player state (2026-11-10)

The comment layer is hit-testable: a right click on a rendered comment opens a
context menu at the pointer, and a 500 ms left hold on a comment blocks its
sender. The three drawing/interaction pieces share one geometry helper set
(`dm_menu_rect`/`dm_menu_row`, `dm_report_rect`/`dm_report_row`) so the engine's
click test and the Direct2D menu cannot disagree.

* **右键菜单**: 点赞 / 举报 / 屏蔽该用户 / 复制内容. 举报 opens the reason
  submenu (`1 垃圾广告`, `2 色情`, `3 引战`, `4 剧透`, `其他`); both remote
  actions show the API `code` and `message` in a toast.
* **点赞** calls `danmaku_thumbup(cid, dmid, 1)`; a `HashSet<String>` of
  in-flight `dmid`s ignores a duplicate click while the first request is open.
* **屏蔽该用户** dedupes `mid_hash` into `settings.blocked_senders`, saves
  `settings.json`, drops that sender's active comments (other senders stay on
  screen) and shows a 3 s 撤销 toast. 撤销 removes the row from
  `settings.blocked_senders`, saves, and reloads the current comment list.
* **复制内容** writes `CF_UNICODETEXT` through `native_player/src/clipboard.rs`.
* **弹幕历史** is a sixth right-strip tab (`历史`). Opening it calls
  `danmaku_history_dates` (the core API already scans the current month plus up
  to 11 earlier ones, so no separate year-month picker is needed); clicking a
  date calls `danmaku_history` and replaces the live comment list in the same
  overlay. The 实时弹幕 row, a second click on the selected date, switching tabs
  or closing the strip restores the live list. A fetch that completes after the
  panel closed is ignored.
* The popup still owns 透明度/字号/速度/区域/屏蔽/分布密度/发送; seek reseeds,
  pause freezes and the layer keeps its independent display-rate cadence
  (`danmaku cadence hz=120 independent=1`).

### Persistence

`native_player/src/state.rs` writes `player_state.json` under
`o11a1::paths::root()`: `volume`, `muted`, `speed`, `aspect`, `brightness`,
`audio_only`, `topmost`, `danmaku_visible`, `quality_qn`,
`quality_codecid` and `audio_index`. 单曲循环 is intentionally absent (it stays
in `settings.json`). The file is loaded before the first `source::resolve`; a
stored QN/codec/audio index only fills a `PlayerConfig` field the caller left
open, so an explicit `--quality` wins and an unavailable tier falls back inside
`source::resolve`. The values are applied as engine fields before
`open_sessions`, which keeps the first DMA/audio start on the stored volume,
mute, speed and aspect instead of reopening a session after construction.

Every change marks a `SaveDebouncer` deadline (450 ms); a burst writes one file
after it settles, and `shutdown` writes unconditionally. Quality/audio switches
also remember the new `(qn, codecid, audio_index)`.

### QA

New `BILI_PLAYER_QA` tokens: `dm_menu`, `dm_hold` (arms the same 500 ms path),
`dm_like`, `dm_report` (submenu), `dm_report_one` (real reason-1 report),
`dm_block`, `dm_copy`, `dm_undo`, `dm_history`, `dm_history_date`,
`dm_history_close`. `qa.ps1` waits for the matching log line (`danmaku menu
open`, `danmaku long-press fired`, `danmaku action kind=…`, `danmaku block
mid_hash=`, `danmaku copy content`, `danmaku history applied`, …).

Verified on `BV1mv4Y6rESW` (3207 live comments) with
`O11A1_HOME` pointing at fresh copies of the saved account:

```powershell
$exe = "$env:TEMP\o11a1-parallel-player2-target\release\native_player.exe"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Bvid BV1mv4Y6rESW -Qa "danmaku,dm_menu" -Seconds 16 `
  -OutPng "$env:TEMP\player2_light_menu.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Bvid BV1mv4Y6rESW -Qa "danmaku,dm_block" -Seconds 14 `
  -OutPng "$env:TEMP\player2_block_undo.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Bvid BV1mv4Y6rESW -Qa "danmaku,dm_history,dm_history_date" -Seconds 30 `
  -OutPng "$env:TEMP\player2_history.png"
powershell -NoProfile -ExecutionPolicy Bypass -File native_player\qa.ps1 -Exe $exe `
  -Bvid BV1mv4Y6rESW -Qa "danmaku,info,dmpopup" -Seconds 16 `
  -OutPng "$env:TEMP\player2_light_toolbar.png"
# same with theme=2: player2_dark_toolbar.png
```

Observed: menu open on a real right click at `640,61`;
`dm_hold` fired at exactly 500 ms, logged
`danmaku blocked sender mid_hash=801067b7`, removed one active comment, and a
`saved` write followed; `dm_undo` then wrote `blocked_total=0` and reloaded
`danmaku loaded n=3209 … view=live`. 点赞 returned `code=0 message=OK`;
举报 reason 1 returned `code=0` with an empty message. 复制内容 left 23 UTF-16
code units in `CF_UNICODETEXT` (read back through ctypes as valid UTF-8).
History found 5 dates for this cid (`2026-11-10` … `2026-11-06`), loaded the
first day as `n=500 source=protobuf`, and restored `n=3207 view=live` on close.
A preset `player_state.json` opened at `qn=64 codec=AVC audio_index=2`,
`speed=1.25`, `aspect=1`, `brightness=0.60`, `audio_only=1`, `topmost=1`,
`danmaku=0`; the mute action wrote the state 453 ms after the change, and a
quality-low switch persisted `qn=16 codecid=7 audio_index=0` and reopened on it.
`cargo test -p native_player` is 47 unit tests + 1 doc-test, all green.

## Honest caveats

* **No Slint types are referenced anywhere in `native_player/`.** It depends on
  `o11a1-core` for `bilibili`/`accounts`/`paths`/settings, and that crate is now
  Slint-free, so no Slint runtime is linked into the native player.
* Anime4K is the shipped 1:1 tier (`Clamp_Highlights` + `Restore_CNN_S`) and not
  the `_M` variant; the x2 upscale stage does not run at the video layer's 1:1
  resolution, exactly as in the shipped player. CAS strength is fixed at 0.6.
* The picture corners are square here; the future main window owns the rounded
  video surface.
* Mini mode is frameless too: the window is exactly 480×270; its top 26 px is
  the drag handle. Entering mini from fullscreen leaves fullscreen and restores
  to the normal window, not back to fullscreen.
* The 信息 description is `暂无简介`: `o11a1::bilibili::VideoDetail` exposes the
  cover/title/owner/views/date/pages but not the video description. The player
  cannot add that field without changing core's public model.
* 选集 page switching itself is still same-decoded-size only; a page whose
  resolution differs is rejected with a toast instead of rebuilding the video
  swapchain. It reopens at 0 and refetches that page's danmaku. The separate
  画质 switch *does* rebuild the video swapchain when the decoded size changes.
* 设置's 画质 and 音轨 rows are interactive. A 画质 switch re-runs playurl on the
  shared session, picks the exact `(qn, codecid)`, allows both resolution changes
  and same-size codec changes, and reopens at the current position. Locked
  大会员 tiers show `需要大会员` in the menu and a toast when clicked. Audio picks
  the DASH index strictly; an unavailable index is reported rather than silently
  replaced. The cost of a switch is one playurl round-trip plus one seek.
* Comment avatars are clipped by a D2D geometric-mask layer and fall back to an
  inset contained square if the driver rejects the layer. Comments are truncated
  to a two-line budget instead of measured per glyph.
* 历史 heartbeat is best-effort: a logged-out session has no `bili_jct` csrf, and
  a risk-control rejection is logged (`heartbeat failed` / `heartbeat rejected`)
  without ever surfacing a playback error. The `resume.json` local resume store
  remains owned by `native_ui::player_launch`; the standalone player does not
  rewrite it.
* Storyboard sheets are decoded at full sheet resolution on the worker and only
  one cropped cell is sent to the overlay; the first hover pays the videoshot
  request plus sheet download/JPEG decode (measured ~1.3 s for a 4800×2700
  sheet on this machine). A failure queues a `storyboard unavailable` line and
  the hover stays time-only.
* 睡眠定时 pauses at expiry; it does not close the window. The 快捷键 row is
  read-only in the player (remapping is still the main UI's settings page), but
  the 11 actions themselves are loaded from `settings.player_keys`.
* The error card's `fault` QA action is an injected error used to verify the
  visible retry UI; the real detection paths (ffmpeg EOF before the tail, audio
  device/session failure, 15 s first-frame timeout) are wired to the same card
  and retry routine.
* The top strip drag uses `WM_NCLBUTTONDOWN/HTCAPTION` and was not exercised by
  the automated harness (only maximize/restore, toolbar, sidebar and close were
  clicked); resize edges are plain `WM_NCHITTEST` and likewise need one manual
  drag/resize check. DWM rounding is requested with `DWMWCP_ROUND`.
* 评论 with the saved account on `BV1mhxo6BEG4` returns 8 top-level comments
  (`total=25, end=0`); the next cursor request returned 0 items and `end=1`, so
  the 加载更多 path is exercised but this upload's next page is empty. The
  热门/时间 buttons issue new `comments(aid, mode, "")` requests.
* `set_speed` is implemented by reopening both streams at the current position
  with ffmpeg `atempo` (0.5×–2.0×). Winning lip-sync on a speed change costs one
  seek; no attempt is made to pitch-shift in real time.
* If no audio device is available the player starts video on the wall clock after
  a short grace period and logs `audio unavailable` / `player av-drift`; there is
  no silent audio track substitute.
* A direct URL with a container ffprobe cannot read has no duration until ffmpeg
  exposes it; seeks are then clamped only by the target itself.
* Screenshot paths follow `o11a1::paths::root()` exactly. When the executable
  lives beside the workspace but its folder is not literally named `target`
  (for example this QA target is `o11a1-player-target`), `root()` resolves to the
  executable folder, so `S` writes under
  `<exe folder>\screenshots\` rather than the repository root.
* `S` uses the existing GDI `BitBlt` capture of the window rect (PrintWindow has
  no surface to read on this window). If another window covers the player, the
  captured PNG shows the occluding pixels; the automated QA harness therefore
  forces the player topmost before pressing `S`. The PNG encode runs on the
  player thread and can cost a few frames in that second, but never pauses
  playback or audio.
* 亮度 goes through the video pixel shader; if `d3dcompiler_47.dll` is missing,
  the player logs `brightness fallback` and presents the plain copy (brightness
  is not applied) rather than failing the frame.
* 仅音频 keeps decoding and presenting black video frames so the pacer, audio
  clock and danmaku timing stay identical; it is not a decode-stop mode.
* In mini, `T` updates the stored 置顶 preference but the window stays topmost;
  the preference is applied on restore.
* The decoded-frame ring holds six pictures. A slow/stalled audio clock can
  therefore delay presentation by up to ~200 ms at 30 fps while the reader is
  backpressured; this is intentional buffering, not a dropped frame. The
  `picture stalled` log (position, queue length, `front_ts`) fires only after
  two seconds without a presented picture.
* Danmaku settings are now live-editable through the toolbar popup and persist
  to `settings.json`. The 发送 box posts through the WBI-signed `dm/post`
  endpoint and reports the API code/message; it does not retry a risk-controlled
  write. Density is runtime-only (not persisted), and the popup's slider drags
  are exercised by the same handlers as the toolbar clicks but were not part of
  the automated click harness.
* The right-click menu and long-press only act while 弹幕 is visible; a short
  click on a comment is deliberately swallowed (it never toggles play/pause).
  A real left hold was armed by the automated Win32 probe and logged
  `danmaku press`, but this shared desktop's synthetic pointer movement
  cancelled it before 500 ms; the deadline/block path is covered by `dm_hold`
  and by `long_press_fires_once_after_threshold_and_cancels_on_drag`. 弹幕历史
  lists dates (the core endpoint already scans 12 months); there is no separate
  year-month picker, and a date with more than one screenful scrolls with the
  normal strip wheel handler.
* The QA pass performed one real thumb-up and one real reason-1 report on the
  dense test upload (`BV1mv4Y6rESW`); both returned `code=0`. These are
  account-authenticated, risk-controlled writes: a non-zero code or an HTTP
  failure is shown verbatim in the toast and never retried automatically.
* The context menu hit-tests the currently drawn comment boxes, not an OCR or
  glyph-level layout; a comment whose heuristic text width is slightly wider
  than its rendered glyphs has a few pixels of slack on the right edge. The
  menu itself is clamped into the client and flips the report submenu left near
  the right edge.
* `player_state.json` is per-home: a packaged player keeps it beside the
  executable, a dev/QA run under `target\...` keeps it at the manifest root,
  and `O11A1_HOME` overrides both. `topmost` is applied once at open; the shell
  can still pin the window itself.
* Scrolling, top and bottom comments each avoid overlap within their own lane
  set, but the three sets share the same vertical band, so an extremely dense
  mix can still cross modes. Text width is a conservative heuristic (CJK ≈ 1 em,
  ASCII ≈ 0.56 em) rather than a full DWrite measurement during lane placement.
* The required upload `BV1mhxo6BEG4` currently exposes only 9 comments for the
  whole 8:19 video, so its screenshot necessarily shows a single scrolling
  comment. The dense 30 fps track run (`BV1mv4Y6rESW`) is the evidence for many
  simultaneous comments and lane separation.
