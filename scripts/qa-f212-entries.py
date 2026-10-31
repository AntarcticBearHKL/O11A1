#!/usr/bin/env python3
"""f212: read one `BILI_ENTRY_CYCLE` run and report the decoder of every round.

    python scripts/qa-f212-entries.py target/qa/f212/f212_entry_search.log 10

The cycle writes one `f212 cycle i=<n>` line *after* closing round n, and every
player session logs its own verdict (`decode=mf` or `decode fallback reason=…`),
so the lines between two markers are exactly one round's session. Nothing is
inferred: a round with no `decode=` line at all is reported as `none`.
"""
import re
import sys

VERDICT = re.compile(r"decode=mf .*?size=(\d+x\d+)")
FALLBACK = re.compile(r"decode fallback reason=(.+)$")
MARK = re.compile(r"f212 cycle i=(\d+).*?open_ms=(\d+).*?close_ms=(\d+)")
OPEN = re.compile(r"player open mode=")


def main() -> int:
    path = sys.argv[1]
    rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 0
    with open(path, encoding="utf-8", errors="replace") as handle:
        lines = handle.read().splitlines()

    segments, current = [], []
    for line in lines:
        if MARK.search(line):
            segments.append(current)
            current = []
            continue
        current.append(line)

    rows, mf, ff, none, extra = [], 0, 0, 0, 0
    for index, segment in enumerate(segments, 1):
        mf_hits = [VERDICT.search(line) for line in segment]
        sizes = [hit.group(1) for hit in mf_hits if hit]
        falls = [FALLBACK.search(line).group(1).strip() for line in segment if FALLBACK.search(line)]
        opens = sum(1 for line in segment if OPEN.search(line))
        mark = next((MARK.search(line) for line in segment[::-1] if MARK.search(line)), None)
        if sizes:
            mf += 1
            verdict = "mf " + ",".join(sizes)
            if falls:
                verdict += " (then " + ",".join(falls) + ")"
        elif falls:
            ff += 1
            verdict = "ffmpeg " + ",".join(f[:48] for f in falls)
        else:
            none += 1
            verdict = "none"
        if opens > 1:
            extra += opens - 1
        rows.append((index, opens, verdict))

    print(f"\n---- f212 {path} ----")
    print(f"{'round':>5}  {'opens':>5}  decode")
    for index, opens, verdict in rows:
        print(f"{index:>5}  {opens:>5}  {verdict}")
    expected = rounds or len(rows)
    ok = mf == expected and ff == 0 and none == 0
    print(
        f"\nrounds={len(rows)}/{expected} mf={mf} ffmpeg={ff} none={none} extra_opens={extra}"
    )
    fallbacks = [line for line in lines if "no_dcomp" in line]
    print("no_dcomp lines:", len(fallbacks))
    for line in fallbacks[:10]:
        print("  ", line)
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
