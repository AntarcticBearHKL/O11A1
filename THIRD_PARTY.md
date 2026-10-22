# Third-party notices

## Fonts

### Chill Round Gothic (å¯’è‰åœ†é»‘ä½“)
- Files: `assets/fonts/ChillRoundGothic_Regular.otf`, `assets/fonts/ChillRoundGothic_Bold.otf`
- Licence: SIL Open Font License 1.1 â€” see `assets/fonts/License.txt`
- Source: https://github.com/Warren2060/ChillRoundGothic
- A rounded derivative of Source Han Sans SC; embedded at compile time and used
  as the UI face.

### Noto Sans SC
- File: `ui/fonts/NotoSansSC-Regular.otf`
- Licence: SIL Open Font License 1.1
- Used as the fallback face for glyphs the rounded face does not cover.

## Rust crates

Dependencies are declared in `Cargo.toml`; their licences are recorded in
`Cargo.lock`'s crates. Notable ones:

- **Slint** (GPL-3.0 / commercial / royalty-free for some uses) â€” UI toolkit.
- **reqwest**, **tokio**, **serde**, **image**, **anyhow**, **windows-sys** and
  others under their respective MIT / Apache-2.0 licences.

## Media tooling

`ffmpeg.exe` / `ffplay.exe` are not distributed with this repository. They are
invoked at runtime and are licensed separately (LGPL/GPL depending on the
build).

## Frame interpolation (optional component)

- **RIFE** (Real-time Intermediate Flow Estimation) ¡ª algorithm by hzwer, MIT.
- **ncnn** ¡ª Tencent, BSD-3-Clause.
- **rife-ncnn-vulkan** model files (`rife-v4.6`) ¡ª nihui, MIT.
- `assets/rife/rife-helper.exe` is a small persistent helper built from the above;
  it ships with the release bundle (not required to build or run the app ¡ª without
  it the RIFE smoothing mode simply reports unavailable and the blend mode is used).
