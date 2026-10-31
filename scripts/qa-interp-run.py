#!/usr/bin/env python3
"""Developer helper: run one scripted playback and summarise the interp lines.

Not part of the product. It exists so an interpolation measurement is one
command instead of a hand-written env block plus a grep:

    python scripts/qa-interp-run.py --seconds 60 --interp 1 --tag base60

Every `interp mode=...` line the pacer writes once a second is parsed into the
fields that matter (`out_hz`, `generated`, `shown_s`, `coal_s`, `drop`, the
blend p50/p95, the UI gap p50/p95 when the probe is on), and the summary prints
median / min / max over the run so a single number can be quoted.
"""

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = os.path.join(ROOT, "target", "release", "o11a1.exe")

FIELDS = {
    "mode": r"mode=(\d+)",
    "generated": r"generated=([\d.]+)/s",
    "out_hz": r"out_hz=(\d+)",
    "avg_ms": r"avg_ms=([\d.]+)",
    "shown": r"shown_s=([\d.]+)/s",
    "coal": r"coal_s=([\d.]+)/s",
    "drop": r"drop=([\d.]+)/s",
    "pool_miss": r"pool_miss=([\d.]+)/s",
    "ui_p50": r"ui_p50_ms=([\d.]+)",
    "ui_p95": r"ui_p95_ms=([\d.]+)",
    "conv_p50": r"conv_p50_ms=([\d.]+)",
    "conv_p95": r"conv_p95_ms=([\d.]+)",
    "hand_p50": r"hand_p50_ms=([\d.]+)",
    "hand_p95": r"hand_p95_ms=([\d.]+)",
    "queue_p50": r"queue_p50_ms=([\d.]+)",
    "queue_p95": r"queue_p95_ms=([\d.]+)",
    "paint_p50": r"paint_p50_ms=([\d.]+)",
    "paint_p95": r"paint_p95_ms=([\d.]+)",
    "present_p50": r"present_p50_ms=([\d.]+)",
    "present_p95": r"present_p95_ms=([\d.]+)",
    "gap_p50": r"gap_p50_ms=([\d.]+)",
    "gap_p95": r"gap_p95_ms=([\d.]+)",
    "total_p50": r"total_p50_ms=([\d.]+)",
    "total_p95": r"total_p95_ms=([\d.]+)",
    "surface_shown": r"surface_shown_s=(\d+)/s",
    "surface_coal": r"surface_coal_s=(\d+)/s",
    "blit_us": r"blit_us=(\d+)",
    "dm_p50": r"dm_p50_ms=([\d.]+)",
    "dm_p95": r"dm_p95_ms=([\d.]+)",
}


def parse(log_path):
    rows = []
    with open(log_path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            if not line.startswith("interp mode="):
                continue
            row = {}
            for key, pattern in FIELDS.items():
                match = re.search(pattern, line)
                if match:
                    row[key] = float(match.group(1))
            if "shown" in row:
                rows.append(row)
    return rows


def summarise(rows, key):
    values = [row[key] for row in rows if key in row]
    if not values:
        return None
    return (statistics.median(values), min(values), max(values))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--seconds", type=int, default=60)
    parser.add_argument("--interp", default="1", help="BILI_INTERP_TEST mode")
    parser.add_argument("--bvid", default="BV1BBnb6aEww")
    parser.add_argument("--tag", default="run")
    parser.add_argument("--four-x", action="store_true")
    parser.add_argument("--no-danmaku", action="store_true")
    parser.add_argument("--no-probe", action="store_true", help="leave BILI_INTERP_PROBE unset")
    parser.add_argument("--env", action="append", default=[], help="EXTRA=value")
    parser.add_argument("--skip-run", action="store_true")
    parser.add_argument("--show", action="store_true", help="print every line")
    args = parser.parse_args()

    log_path = os.path.join(ROOT, "target", f"interp-{args.tag}.log")
    if not args.skip_run:
        env = dict(os.environ)
        env["BILI_DEBUG_LOG"] = log_path
        env["BILI_INTERP_TEST"] = args.interp
        if not args.no_probe:
            env["BILI_INTERP_PROBE"] = "1"
        env["BILI_OPEN_BVID"] = args.bvid
        env["BILI_SOAK_TEST"] = str(args.seconds)
        if args.four_x:
            env["BILI_INTERP_4X"] = "1"
        if args.no_danmaku:
            env["BILI_QA_NO_DANMAKU"] = "1"
        for pair in args.env:
            key, _, value = pair.partition("=")
            env[key] = value
        if os.path.exists(log_path):
            os.remove(log_path)
        started = time.time()
        result = subprocess.run([EXE], env=env, cwd=ROOT)
        elapsed = time.time() - started
        print(f"exit={result.returncode} wall_s={elapsed:.1f} log={log_path}")

    rows = parse(log_path)
    if not rows:
        print("no interp lines")
        return 1
    if args.show:
        for row in rows:
            print(json.dumps(row, sort_keys=True))
    print(f"lines={len(rows)}")
    header = f"{'field':14} {'median':>8} {'min':>8} {'max':>8}"
    print(header)
    for key in FIELDS:
        summary = summarise(rows, key)
        if summary is None:
            continue
        print(f"{key:14} {summary[0]:8.2f} {summary[1]:8.2f} {summary[2]:8.2f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
