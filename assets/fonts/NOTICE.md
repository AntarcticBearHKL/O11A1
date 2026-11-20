# Bundled UI fonts

The app ships **Chill Round Gothic (寒蝉圆黑体)** as the single UI typeface,
including the native player and danmaku overlay. It is a rounded cut of Source
Han Sans SC, so one family covers both the renderer and the DirectWrite player.

## Chill Round Gothic

- Upstream: **ChillRoundGothic** by Warren2060 — <https://github.com/Warren2060/ChillRoundGothic>
- Files: `ChillRoundGothic_Regular.ttf` (400), `ChillRoundGothic_Medium.ttf`
  (500), `ChillRoundGothic_Bold.ttf` (700)
- Registered family name: `Chill Round Gothic` (the fonts share the typographic
  family from name ID 16; the individual files name their weights separately)
- License: SIL Open Font License 1.1, see `ChillRoundGothic-OFL.txt`
- Coverage: 27,183 cmap entries, a rounded Source Han Sans SC derivative that
  covers the common simplified set including 巽.

The files are unmodified upstream releases. Browser and DirectWrite both resolve
the family by its Latin name; the CSS family and the native player use the same
string so the two renderers cannot drift apart.

## Where the files live

- `public/fonts/` — source for the renderer; Vite copies it to `dist/fonts/`.
- `assets/fonts/` — local development copy and licenses; its `.ttf` files are
  excluded from the packaged asar (`!assets/fonts/*.ttf`) so the fonts are not
  shipped three times.
- `runtime/assets/fonts/` — copied from `public/fonts/` by
  `scripts/build-engine.ps1`; the native player loads them privately with
  `AddFontResourceExW` and never installs them system-wide.
