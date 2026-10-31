# f211 report: turn the QA logs into the numbers the acceptance asks for.
#
#   python scripts/qa-f211-report.py target/qa/f211/*.log
#   python scripts/qa-f211-report.py --seek target/qa/f211/f211_seek_mf_dc1_*.log
#
# Reads only the lines the player already writes:
#   seek trace kind=… total_ms=… ui_ms=… teardown_ms=… setup_ms=… decoder_ms=… audio_ms=… present_ms=…
#   seek served target_ms=… first_frame_ms=…
#   avsync tag=… t=… phase=… presented_ms=… audio_ms=… delta_ms=… paused=…
#   session first present qn=… switch_qn=… switch_after_ms=… matched=…
import sys
import glob
import re
import statistics

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:  # noqa: BLE001 - a console without reconfigure still prints
    pass


def pct(values, p):
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    # Nearest-rank percentile: the k-th smallest, k = ceil(p * n).
    k = max(1, min(len(ordered), int(-(-p * len(ordered) // 1))))
    return ordered[k - 1]


def parse_int(pattern, line, default=None):
    match = re.search(pattern, line)
    if not match or match.group(1) == "-":
        return default
    return int(match.group(1))


def seek_rows(paths):
    rows = []
    for path in paths:
        for line in open(path, encoding="utf-8", errors="replace"):
            if "seek trace " not in line:
                continue
            row = {
                "log": path.split("/")[-1],
                "kind": (re.search(r"kind=(\S+)", line) or [None, "?"])[1],
                "total": parse_int(r"total_ms=(\d+)", line),
                "ui": parse_int(r"ui_ms=(\d+|-)", line),
                "teardown": parse_int(r"teardown_ms=(\d+|-)", line),
                "setup": parse_int(r"setup_ms=(\d+|-)", line),
                "decoder": parse_int(r"decoder_ms=(\d+|-)", line),
                "audio": parse_int(r"audio_ms=(\d+|-)", line),
                "present": parse_int(r"present_ms=(\d+|-)", line),
            }
            rows.append(row)
    return rows


def avsync_rows(paths):
    out = {}
    for path in paths:
        for line in open(path, encoding="utf-8", errors="replace"):
            if "avsync " not in line or "delta_ms=-" in line:
                continue
            if "paused=1" in line:
                continue
            phase = (re.search(r"phase=(\w+)", line) or [None, "?"])[1]
            delta = parse_int(r"delta_ms=(-?\d+)", line)
            if delta is None:
                continue
            out.setdefault(phase, []).append(delta)
    return out


def quality_rows(paths):
    rows = []
    for path in paths:
        for line in open(path, encoding="utf-8", errors="replace"):
            if "session first present " not in line:
                continue
            rows.append(
                {
                    "log": path.split("/")[-1],
                    "qn": parse_int(r"qn=(\d+)", line),
                    "want": parse_int(r"switch_qn=(\d+)", line),
                    "after": parse_int(r"switch_after_ms=(\d+)", line),
                    "matched": "matched=1" in line,
                }
            )
    return rows


def show_seek(rows):
    if not rows:
        return
    print("== seek ==")
    print(f"{'log':42} {'kind':10} {'total':>5} {'ui':>4} {'tear':>4} {'setup':>5} "
          f"{'dec':>4} {'aud':>4} {'present':>7}")
    for row in rows:
        def cell(key):
            value = row[key]
            return "-" if value is None else str(value)

        print(f"{row['log']:42} {row['kind']:10} {cell('total'):>5} {cell('ui'):>4} "
              f"{cell('teardown'):>4} {cell('setup'):>5} {cell('decoder'):>4} "
              f"{cell('audio'):>4} {cell('present'):>7}")
    totals = [r["total"] for r in rows if r["total"] is not None]
    print()
    print(f"n={len(totals)} p50={pct(totals, 0.50)}ms p95={pct(totals, 0.95)}ms "
          f"min={min(totals)}ms max={max(totals)}ms mean={statistics.mean(totals):.1f}ms")
    # p50/p95 again without the first seek of each log (the one that pays the
    # sink's first draw / the player's start-up): the steady-state number.
    steady = []
    for path in {r["log"] for r in rows}:
        first = True
        for row in rows:
            if row["log"] != path or row["total"] is None:
                continue
            if first:
                first = False
                continue
            steady.append(row["total"])
    if steady:
        print(f"steady (all but the first seek of each log): n={len(steady)} "
              f"p50={pct(steady, 0.50)}ms p95={pct(steady, 0.95)}ms max={max(steady)}ms")
    print()


def show_avsync(groups):
    if not groups:
        return
    print("== A/V sync (audio clock - presented picture, ms) ==")
    print(f"{'phase':8} {'n':>5} {'p50':>6} {'p95':>6} {'min':>6} {'max':>6} {'|max|':>6}")
    for phase in sorted(groups):
        values = groups[phase]
        print(f"{phase:8} {len(values):>5} {pct(values, 0.5):>6} {pct(values, 0.95):>6} "
              f"{min(values):>6} {max(values):>6} {max(abs(v) for v in values):>6}")
    print()


def show_quality(rows):
    if not rows:
        return
    print("== quality switch ==")
    for row in rows:
        print(f"{row['log']:42} qn={row['qn']:<4} want={row['want']:<4} "
              f"after_ms={row['after']} matched={int(row['matched'])}")
    print()


def main(argv):
    paths = []
    mode = "all"
    for arg in argv[1:]:
        if arg.startswith("--"):
            mode = arg[2:]
        else:
            paths.extend(glob.glob(arg))
    if not paths:
        print(__doc__)
        return 1
    if mode in ("all", "seek"):
        show_seek(seek_rows(paths))
    if mode in ("all", "avsync"):
        show_avsync(avsync_rows(paths))
    if mode in ("all", "quality"):
        show_quality(quality_rows(paths))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
