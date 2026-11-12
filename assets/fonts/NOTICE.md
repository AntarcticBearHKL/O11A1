# Bundled UI font — `O11A1 Round`

`O11A1Round-Regular.ttf` / `O11A1Round-Bold.ttf` are the app's single UI face.
They are the **regular and bold weights of Resource Han Rounded CN (资源圆体 CN)
v0.990**, subset and slightly modified; the family is renamed because a modified
OFL font must not carry the upstream family name.

## Upstream

- **Resource Han Rounded CN 0.990** — a rounded derivative of Source Han Sans,
  by Cyano Hao. <https://github.com/CyanoHao/Resource-Han-Rounded>
  Licensed under the SIL Open Font License 1.1, see `OFL-License.txt`
  (it carries Adobe's Reserved Font Name `Source`, which this file does not use).
- Nine symbol glyphs the upstream face does not have — `▭ ▮ ▯ ▴ ▸ ▾ ◂ ✕ ✗`
  (U+25AD U+25AE U+25AF U+25B4 U+25B8 U+25BE U+25C2 U+2715 U+2717) — were copied
  from **Noto Sans Symbols 2** (Copyright 2022 The Noto Project Authors),
  also SIL OFL 1.1, see `OFL-NotoSansSymbols2.txt`.

## What was changed

1. Subset to what a Chinese desktop client can be asked to draw: full CJK
   Unified Ideographs (U+4E00–9FFF), ASCII/Latin-1/Extended-A, general
   punctuation, arrows, geometric shapes, box drawing, circled numbers, kana,
   CJK symbols and punctuation, half/full-width forms. GB2312 (6763 chars) and
   every non-ASCII character in `ui/` and `src/` are covered — 0 missing.
2. Vertical metrics pinned the way the previous UI face had them:
   `fsSelection |= USE_TYPO_METRICS` with `sTypoAscender/Descender/LineGap =
   880 / -120 / 0`, i.e. a 1.000 em line box. Every `Text` in `ui/app.slint`
   keeps the line height it was laid out against; the CJK glyph bounds (max
   yMax 856, min yMin -95) sit inside it with room to spare.
3. Family renamed to `O11A1 Round`.

## Why the family name is not the upstream one

OFL 1.1 §3: a modified version may not use a Reserved Font Name, and should not
be distributed under the original name without permission. Renaming also keeps
the app's embedded face from ever resolving against a user-installed build of
the same family with a different weight set.

Rebuild: `build_fonts.py` (fontTools; the same pyftsubset call is documented in
the header of that script). Binary size: 10.60 MB + 10.10 MB.
