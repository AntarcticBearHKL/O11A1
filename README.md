# O11A1

A Bilibili desktop client written in Rust with a [Slint](https://slint.dev) UI.
Frameless windows, frosted glass, a hardware-decoded video player driven by
ffmpeg, and a network layer that picks the fastest CDN mirror before playing.

## Features

**Home**
- Feed with row-virtualised cards (only visible rows are instantiated)
- Card meta designed for scanning: one-line title (full title on hover), the
  uploader's name (click opens their page), a play-count *rarity dot* instead of
  a number, and a freshness chip whose colour follows an exponential scale
  (1 h ‚Üí 5 h ‚Üí 12 h ‚Üí 1 d ‚Üí 1 week ‚Üí 1 month ‚Üí 1 quarter ‚Üí 1 year ‚Üí older)
- Search collapses to a single button and expands into a field on click
- Background preload ring (default 2 batches, configurable 0‚Äì4) so a refresh
  paints instantly, covers included
- Favourites recommendation: a Pinterest-style masonry built from your folders,
  their videos and each video's related list

**Player**
- Chrome that follows the pointer: the toolbar and window buttons appear only
  while the pointer is inside the *video window* (each window tracks its own)
- Compact centred toolbar; the top third is the seek bar
- Right-click the seek bar to morph it into a 20-cell danmaku-density heatmap;
  double-click a cell to zoom to 1/20, right-click to zoom back out
- Vertical iOS-style volume slider; the wheel over the picture changes volume
- Playback queue with three advance modes: manual / auto / shuffle, plus a
  spinning vinyl in the home bar
- Fullscreen panels sample the *video* for their frosted glass; windowed panels
  sample the desktop
- Comments: threaded replies, sortable by hot / time, infinite scroll with
  one-page-ahead prefetch

**Network / playback**
- CDN classification (mirror / object-storage / self-built / PCDN) and a
  sustained-throughput picker; the winner is cached for 10 minutes
- Optional loopback prefetch proxy: parallel range fetches across mirrors with
  real byte accounting (the profiler shows metered MB/s, not an estimate)
- Seek warm points (default 10, configurable 4‚Äì20): an evenly spaced chunk of
  each position is pre-fetched so dragging lands on cached bytes
- Decoding runs at the display size (up to 1920√ó1080), never a fixed 540p
- Per-process audio volume, so changing volume never restarts the audio child

**Other**
- Minimise to taskbar / hide to tray / quit, chosen in Settings
- Playback quality matrix: every DASH stream the account can receive, each with
  resolution, real frame rate, codec and bitrate
- Remembers the last player window position and size
- Global hotkey `Ctrl+Shift+B` toggles the main window

## Build

Requires Windows, Rust 1.80+ (MSVC toolchain) and the Visual Studio C++ tools.
The build uses `lld-link` when present (see `.cargo/config.toml`).

```powershell
cargo build --release
# -> target\release\o11a1.exe
```

Run the test suite:

```powershell
cargo test --release
```

## Runtime notes

- `ffmpeg.exe` / `ffplay.exe` are expected next to the executable (or on `PATH`).
- Settings, accounts and tokens live in `settings.json` / `accounts.json` /
  `token.json` beside the executable and are **not** part of this repository.
- Optional diagnostics (all off by default): `BILI_DEBUG_LOG=<path>`,
  `BILI_AUTOPLAY=1`, `BILI_SETTINGS_ON_START=1`, `BILI_PROFILER_LOG=1`.

## Keyboard

| Key | Action |
| --- | --- |
| Space | play / pause |
| ‚Üê / ‚Üí | seek back / forward |
| ‚Üë / ‚Üì | volume |
| M | mute |
| F | fullscreen |
| D | danmaku on/off |
| S | screenshot |

The player keys are remappable in Settings ‚Üí Âø´Êç∑ÈîÆ.

## Third-party

See [THIRD_PARTY.md](THIRD_PARTY.md). The UI font is Chill Round Gothic (OFL-1.1),
bundled under `assets/fonts/` with its licence text.

## Frame smoothing

Settings °˙ ”≈ªØ / the player's ∆Ωª¨ button cycles three modes:

| Mode | What it does | Limits |
| --- | --- | --- |
| πÿ±’ | native frame rate | °™ |
| ªÏ∫œ | blends adjacent frames to the display rate (display-only, no A/V impact) | 1080p capped at 60 fps; 120 fps only at °‹960°¡540 |
| RIFE | optical-flow interpolation through the bundled helper (real intermediate frames) | **°‹1280°¡720, 30/25 fps sources only**; falls back to ªÏ∫œ above that or if the helper is missing |

RIFE needs `rife-helper.exe` + `assets/rife/model` next to the exe (both are in the
release bundle). Measured on an RTX 4060 Ti: 16®C18 ms per 720p pair (°÷58 pairs/s).
