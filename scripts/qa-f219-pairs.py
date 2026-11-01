#!/usr/bin/env python3
"""f219: align every presented slot's `pts` with the luma it actually drew.

The QA sample (`scripts/qa-f218-flicker.ps1` generates it) is a ramp of flat
luma levels, one level per source frame:

    geq=lum='8+8*mod(N,30)'

so for every source frame there is exactly one expected grey level, and the
sequence of levels is a 30-frame sawtooth. `BILI_F218_SLOTS=1` makes the sink
log one line per presented slot:

    f218 slot pts=10999 luma=84 kind=warp pair=123 phase=0.50 warped=1

`pts` is the source frame the slot *picked*, `luma` is the mean of the pixels it
*actually drew* (a readback of the backbuffer right before the compositor gets
it). If the picture a slot draws is the picture it picked, `luma` must equal the
ramp level at `pts`. Any other value means the surface held a *different* frame -
the f218 闪烁, and the last 0.7 % of it that f219 fixes (a surface released too
early, so the decoder wrote a later frame into the same array slice).

Usage:

    python scripts/qa-f219-pairs.py target/qa/f218/f218_1080_1080p_interp3_*.log
    python scripts/qa-f219-pairs.py --tolerance 5 --quiet <log>

Exit code 0 when no pair is wrong, 1 otherwise (so a QA run can gate on it).
"""

import argparse
import glob
import re
import statistics
import sys

SLOT = re.compile(
    r"f218 slot pts=(\d+) luma=(-?\d+(?:\.\d+)?) kind=(\S+) pair=(\d+) phase=([\d.]+)"
)
# The sample's own ramp: Y' 8 + 8*N (mod 30), studio range, and the video
# processor's studio->full conversion is the only thing between it and the
# backbuffer luma this script reads.
RAMP_STUDIO_BASE = 8.0
RAMP_STUDIO_STEP = 8.0
STUDIO_TO_FULL = 255.0 / 219.0


def ramp_luma(frame: int) -> float:
    """Expected backbuffer luma of ramp frame `frame` (any integer)."""
    value = RAMP_STUDIO_BASE + RAMP_STUDIO_STEP * (frame % 30)
    return min(255.0, max(0.0, (value - 16.0) * STUDIO_TO_FULL))


def parse(paths):
    """[(pts, luma, pair)] in log order, plus the slot kind histogram."""
    slots = []
    kinds = {}
    for path in paths:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                if "f218 slot" not in line:
                    continue
                match = SLOT.search(line)
                if not match:
                    continue
                pts = int(match.group(1))
                luma = float(match.group(2))
                kind = match.group(3)
                pair = int(match.group(4))
                slots.append((pts, luma, pair))
                kinds[kind] = kinds.get(kind, 0) + 1
    return slots, kinds


def group_pairs(slots):
    """One row per decoded pair: its pts and the median luma its slots drew."""
    rows = []
    current = None
    for pts, luma, pair in slots:
        if current is None or current["pair"] != pair:
            current = {"pair": pair, "pts": pts, "luma": [], "kinds": {}}
            rows.append(current)
        current["luma"].append(luma)
    for row in rows:
        row["slots"] = len(row["luma"])
        row["luma"] = statistics.median(row["luma"])
    return rows


def frame_step(rows):
    """ms per source frame, from the pts gaps between consecutive pairs."""
    gaps = [
        b["pts"] - a["pts"]
        for a, b in zip(rows, rows[1:])
        if 0 < b["pts"] - a["pts"] < 200
    ]
    return statistics.median(gaps) if gaps else 33.4


def fit(rows):
    """Find (fps, offset) that maps `pts` to the ramp's own frame numbering.

    Two unknowns, because the stream's first sample is not ramp frame 0 (a mux
    or seek offset moves it) and the exact frame duration has to be recovered
    from ms-truncated pts values (33.33 ms shows up as 33/34 alternating). Drift
    of even 0.3 ms per frame is five frames over a 15 s run, so both are fitted
    rather than assumed; a poor fit would make every pair look wrong.
    """
    best = None
    for hundredths in range(2900, 3101, 2):
        fps = hundredths / 100.0
        for offset in range(30):
            hits = 0
            for row in rows:
                frame = round(row["pts"] * fps / 1000.0) - offset
                if abs(row["luma"] - ramp_luma(frame)) <= 3.0:
                    hits += 1
            if best is None or hits > best[0]:
                best = (hits, fps, offset)
    return best[0], best[1], best[2]


def analyse(rows, fps, offset, tolerance):
    """Per-pair deviation, in frames, from the level its pts calls for."""
    results = []
    for row in rows:
        frame = round(row["pts"] * fps / 1000.0) - offset
        expected = ramp_luma(frame)
        error = row["luma"] - expected
        results.append(
            {
                "pair": row["pair"],
                "pts": row["pts"],
                "luma": row["luma"],
                "expected": expected,
                "slots": row["slots"],
                "frames_off": error / (RAMP_STUDIO_STEP * STUDIO_TO_FULL),
                "wrong": abs(error) > tolerance,
            }
        )
    return results


def monotonic(results):
    """pts must never move backwards; repeats and skips are counted apart.

    One pair id is one presentation interval, not one frame: a slow first frame
    (or a pause) shows the same source frame for two pairs, and a hiccup can skip
    one. Neither is a frame-order error, so only a rewind fails the run.
    """
    rewinds = []
    repeats = 0
    skips = 0
    for previous, current in zip(results, results[1:]):
        delta = current["pts"] - previous["pts"]
        if delta < 0:
            rewinds.append((current["pair"], previous["pts"], current["pts"]))
        elif delta == 0:
            repeats += 1
        elif delta > 70:
            skips += 1
    return rewinds, repeats, skips


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", help="BILI_DEBUG_LOG file(s), globs allowed")
    parser.add_argument(
        "--tolerance",
        type=float,
        default=5.0,
        help="luma tolerance for 'the right frame' (default 5, ~half a ramp step)",
    )
    parser.add_argument("--quiet", action="store_true", help="only the summary")
    parser.add_argument(
        "--pairs",
        type=int,
        default=0,
        help="only the first N pairs (a window comparable to an earlier run)",
    )
    parser.add_argument("--limit", type=int, default=12, help="wrong pairs to list")
    args = parser.parse_args(argv)

    paths = []
    for pattern in args.logs:
        matched = glob.glob(pattern)
        paths.extend(matched if matched else [pattern])
    if not paths:
        print("没有日志")
        return 1

    slots, kinds = parse(paths)
    if not slots:
        print("日志里没有 `f218 slot`（BILI_F218_SLOTS=1 没开？）")
        return 1
    rows = group_pairs(slots)
    if args.pairs > 0:
        rows = rows[: args.pairs]
    step = frame_step(rows)
    hits, fps, offset = fit(rows)
    results = analyse(rows, fps, offset, args.tolerance)
    wrong = [row for row in results if row["wrong"]]
    rewinds, repeats, skips = monotonic(results)

    print(f"logs={len(paths)} slots={len(slots)} pairs={len(rows)}")
    print(
        f"frame_step={step:.2f} ms fit=fps {fps:.2f} offset={offset}"
        f" 拟合命中={hits}/{len(rows)} ({100.0 * hits / max(1, len(rows)):.1f} %)"
        f" tolerance={args.tolerance}"
    )
    print("slot kinds: " + " ".join(f"{k}={v}" for k, v in sorted(kinds.items())))

    if not args.quiet:
        print("\n---- wrong pairs (drew a different frame than it picked) ----")
        if not wrong:
            print("(none)")
        for row in wrong[: args.limit]:
            print(
                "pair={pair} pts={pts} luma={luma:.1f} expected={expected:.1f}"
                " off={off:+.1f} frames slots={slots}".format(
                    off=row["frames_off"], **row
                )
            )
        if len(wrong) > args.limit:
            print(f"... {len(wrong) - args.limit} more")

    print("\n---- pts monotonicity ----")
    print(f"repeats={repeats} (same source frame for two pairs) skips={skips} (>2 frames)")
    if not rewinds:
        print("no pair moved back: ok")
    for pair, before, after in rewinds[: args.limit]:
        print(f"pair={pair} pts {before} -> {after} (rewind)")

    worst = max((abs(row["frames_off"]) for row in results), default=0.0)
    print(
        f"\nwrong_pairs={len(wrong)}/{len(rows)}"
        f" ({100.0 * len(wrong) / max(1, len(rows)):.1f} %)"
        f" rewinds={len(rewinds)} worst_offset_frames={worst:.1f}"
    )
    verdict = "PASS" if not wrong and not rewinds else "FAIL"
    print(f"f219 pairs: {verdict}")
    return 0 if verdict == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
