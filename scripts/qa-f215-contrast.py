# f215: text contrast on the wallpaper-sampled surfaces.
# Developer tool, not product code.
#
#   python scripts/qa-f215-contrast.py --shots target/qa/f215
#
# The panels no longer sit on one flat tone: each one paints its own patch of the
# generated wallpaper (bright cyan/pink in dark, pale lavender/mint in light).
# This script recovers the wallpaper tone under the sampled surfaces from the
# capture itself and reports the WCAG contrast of the ink tokens against the
# BRIGHTEST and DARKEST patch - i.e. the two worst cases the owner asked for.
#
# How the wallpaper tone is recovered: the window paints the wallpaper, then the
# `window-bg` film (45 % wallpaper / 55 % tint) over it. Empty gaps between cards
# therefore show  `obs = 0.55 * tint + 0.45 * wall`, which inverts exactly. The
# gap band is the 8 px ring around every card, sampled on the capture.
#
# The composite a card shows is  `card_bg = a * tint + (1 - a) * wall`, with
# `a` = AppTheme.glass-tint's alpha. Ink contrast is then the WCAG ratio of
# AppTheme.text / text-dim / text-soft against the p1 and p99 card_bg.

import argparse
import os
import sys

import numpy as np
from PIL import Image

# AppTheme tokens (ui/app.slint) that the check covers.
TINT = {
    "dark": {"tint": (0x1E, 0x1E, 0x1E), "alpha": 0xB3 / 255.0, "window": (0x12, 0x12, 0x12),
             "ink": {"text": (0xE8, 0xE8, 0xE8), "text-dim": (0xAA, 0xAA, 0xAA),
                     "text-soft": (0xAC, 0xAC, 0xAC), "text-body": (0xC8, 0xC8, 0xC8)}},
    "light": {"tint": (0xFF, 0xFF, 0xFF), "alpha": 0x8F / 255.0, "window": (0xF5, 0xF5, 0xF7),
              "ink": {"text": (0x1A, 0x1A, 0x1A), "text-dim": (0x5A, 0x5A, 0x62),
                      "text-soft": (0x55, 0x55, 0x55), "text-body": (0x33, 0x33, 0x33)}},
}

# Empty wallpaper patches in the 1280x800 home capture, found by scanning the
# capture for rows that are flat across the grid (card rows carry text and
# covers): the strip above the first row and the two gaps between the rows.
# Nothing here is a card, a control or the bottom bar.
GAPS = [
    (4, 70, 340, 900),      # above row 1 (right of the avatar, left of the buttons)
    (300, 314, 40, 1240),   # between row 1 and row 2
    (540, 552, 40, 1240),   # between row 2 and row 3
]


def luma(rgb):
    return 0.2126 * rgb[..., 0] + 0.7152 * rgb[..., 1] + 0.0722 * rgb[..., 2]


def linear(channel):
    c = channel / 255.0
    return np.where(c <= 0.04045, c / 12.92, ((c + 0.055) / 1.055) ** 2.4)


def rel_luma(rgb):
    r, g, b = (linear(np.asarray(rgb[i], dtype=np.float64)) for i in range(3))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def ratio(a, b):
    la, lb = rel_luma(a), rel_luma(b)
    hi, lo = max(la, lb), min(la, lb)
    return (hi + 0.05) / (lo + 0.05)


def recover_wallpaper(image, theme):
    """Wallpaper tone (0..255 float RGB) from the gap bands of one capture."""
    cfg = TINT[theme]
    pixels = np.asarray(image.convert("RGB"), dtype=np.float64)
    samples = []
    for y0, y1, x0, x1 in GAPS:
        band = pixels[y0:y1, x0:x1].reshape(-1, 3)
        if band.size:
            samples.append(band)
    gaps = np.concatenate(samples, axis=0)
    # obs = 0.55 * window + 0.45 * wall  ->  wall = (obs - 0.55*window) / 0.45
    wall = (gaps - 0.55 * np.asarray(cfg["window"], dtype=np.float64)) / 0.45
    return np.clip(wall, 0, 255)


def seam_report(path, seam_x=512):
    """Largest horizontal luma step in a wallpaper-only band.

    The virtual tile repeats every 1024 logical px, so a wrong UV wrap would put
    a hard vertical edge at the tile boundary INSIDE a sampled surface. The seam
    capture parks the window at (512, 256), i.e. the boundary is at x = 512; a
    correct wrap leaves the step there indistinguishable from the wallpaper's own
    dither (< 1 luma unit), while a broken wrap (double pixel, offset by a tile)
    shows up as a step of many units.
    """
    image = np.asarray(Image.open(path).convert("RGB"), dtype=np.float64)
    bands = {
        "card body (row 1)": (246, 256, 342, 638),
        "card body (row 2)": (490, 500, 342, 638),
        "gap between rows": (302, 312, 40, 1240),
    }
    bad = False
    for label, (y0, y1, x0, x1) in bands.items():
        band = image[y0:y1, x0:x1].mean(axis=0)
        lum = 0.2126 * band[:, 0] + 0.7152 * band[:, 1] + 0.0722 * band[:, 2]
        step = np.abs(np.diff(lum))
        median = float(np.median(step))
        if x0 <= seam_x <= x1:
            at_seam = float(step[seam_x - x0 - 1])
        else:
            at_seam = float("nan")
        ok = not (at_seam > median * 6.0 + 0.5)
        bad = bad or not ok
        print("  %-20s step@seam=%5.2f  median=%.3f  max=%.2f at x=%d  %s"
              % (label, at_seam, median, step.max(), x0 + int(step.argmax()),
                 "ok" if ok else "SEAM"))
    return bad


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    parser = argparse.ArgumentParser()
    parser.add_argument("--shots", default=os.path.join(root, "target", "qa", "f215"))
    parser.add_argument("--seam", action="store_true",
                        help="only report the tile-wrap step of the *seam* captures")
    args = parser.parse_args()

    if args.seam:
        bad = False
        for theme in ("light", "dark"):
            for tag in ("f215_seam2_%s.png", "f215_seam_%s.png"):
                path = os.path.join(args.shots, tag % theme)
                if not os.path.exists(path):
                    continue
                print("\n=== tile wrap, %s" % os.path.basename(path))
                bad = seam_report(path) or bad
        print("\n%s" % ("FAIL: a tile boundary is visible" if bad else
                        "no visible tile boundary at x=512"))
        return 1 if bad else 0

    failed = False
    for theme in ("dark", "light"):
        path = os.path.join(args.shots, "f215_home_cards_%s.png" % theme)
        if not os.path.exists(path):
            print("skip %s (no %s)" % (theme, path))
            continue
        wall = recover_wallpaper(Image.open(path), theme)
        cfg = TINT[theme]
        print("\n=== %s wallpaper patch recovered from %s" % (theme, os.path.basename(path)))
        # The two worst cases: the brightest and the darkest patch of the sheet.
        # Percentiles, not min/max: one stray antialiased pixel must not decide a
        # contrast number.
        lum = luma(wall)
        order = np.argsort(lum)
        bright = wall[order[int(len(order) * 0.98):]].mean(axis=0)
        dark = wall[order[: max(1, int(len(order) * 0.02))]].mean(axis=0)
        print("  patch bright=#%02x%02x%02x  dark=#%02x%02x%02x"
              % (int(bright[0]), int(bright[1]), int(bright[2]),
                 int(dark[0]), int(dark[1]), int(dark[2])))
        for label, patch in (("bright", bright), ("dark", dark)):
            card = cfg["alpha"] * np.asarray(cfg["tint"], dtype=np.float64) + \
                (1.0 - cfg["alpha"]) * patch
            print("  card over %-6s = #%02x%02x%02x"
                  % (label, int(card[0]), int(card[1]), int(card[2])))
            for name, ink in cfg["ink"].items():
                value = ratio(tuple(card), ink)
                ok = value >= 4.5
                if not ok:
                    failed = True
                print("    %-10s %5.2f:1  %s" % (name, value, "ok" if ok else "LOW"))
    print("\n%s" % ("FAIL: an ink token is below 4.5:1 on a card patch" if failed else
                    "all sampled-card ink ratios >= 4.5:1"))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
