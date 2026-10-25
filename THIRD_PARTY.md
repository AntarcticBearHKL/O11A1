# Third-party notices

## Fonts

### Chill Round Gothic (寒蝉圆黑体)
- Files: `assets/fonts/ChillRoundGothic_Regular.otf`, `assets/fonts/ChillRoundGothic_Bold.otf`
- Licence: SIL Open Font License 1.1 — see `assets/fonts/License.txt`
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

- **Slint** (GPL-3.0 / commercial / royalty-free for some uses) — UI toolkit.
- **reqwest**, **tokio**, **serde**, **image**, **anyhow**, **windows-sys** and
  others under their respective MIT / Apache-2.0 licences.
- **rusqlite** (MIT) with the `bundled` feature — the 养鱼 engine's embedded
  storage (`src/fish.rs`). The bundled SQLite amalgamation is in the public
  domain (https://sqlite.org/copyright.html) and is compiled into the binary.

## Media tooling

`ffmpeg.exe` / `ffplay.exe` are not distributed with this repository. They are
invoked at runtime and are licensed separately (LGPL/GPL depending on the
build).
