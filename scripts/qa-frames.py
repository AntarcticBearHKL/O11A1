#!/usr/bin/env python3
"""QA helper: dump two consecutive 1080p frames of a Bilibili video as raw RGBA.

Developer tool for the f189 interpolation report, not part of the product. The
frames are what the player's own decoder would see: the highest-quality DASH
video track, decoded with ffmpeg at its native size.

    python scripts/qa-frames.py BV1BBnb6aEww 43058203389 42
    # -> %TEMP%/f189/frame_a.rgba, frame_b.rgba (1920*1080*4 bytes each)

`sid` is the cid (from the player's `stream` line, or `view` API `cid`); `at` is
the timestamp in seconds of the first frame.
"""

import json
import os
import subprocess
import sys
import tempfile
import urllib.parse
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"


def cookies():
    with open(os.path.join(ROOT, "accounts.json"), encoding="utf-8") as handle:
        token = json.load(handle)
    return "; ".join(f"{c['name']}={c['value']}" for c in token["cookies"])


def best_video(bvid, cid, cookie):
    query = urllib.parse.urlencode(
        {"bvid": bvid, "cid": cid, "fnval": 16, "fourk": 1, "fnver": 0}
    )
    request = urllib.request.Request(
        f"https://api.bilibili.com/x/player/playurl?{query}",
        headers={"User-Agent": UA, "Cookie": cookie, "Referer": "https://www.bilibili.com/"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        data = json.load(response)
    if data.get("code") != 0:
        raise SystemExit(f"playurl failed: {data}")
    videos = data["data"]["dash"]["video"]
    track = max(videos, key=lambda v: (v["id"], v["bandwidth"]))
    return track, data["data"]["dash"].get("duration")


def main():
    bvid = sys.argv[1]
    cid = int(sys.argv[2])
    at = float(sys.argv[3]) if len(sys.argv) > 3 else 42.0
    out = os.path.join(tempfile.gettempdir(), "f189")
    os.makedirs(out, exist_ok=True)
    cookie = cookies()
    track, duration = best_video(bvid, cid, cookie)
    print(f"track qn={track['id']} codecid={track['codecid']} bandwidth={track['bandwidth']} duration={duration}")
    base = track["baseUrl"]
    raw = os.path.join(out, "frames.rgba")
    cmd = [
        "ffmpeg", "-v", "error", "-y",
        "-headers", f"Referer: https://www.bilibili.com/\r\nUser-Agent: {UA}\r\nCookie: {cookie}\r\n",
        "-ss", str(at), "-i", base, "-frames:v", "2",
        "-pix_fmt", "rgba", "-f", "rawvideo", raw,
    ]
    subprocess.run(cmd, check=True)
    size = os.path.getsize(raw)
    frame = 1920 * 1080 * 4
    if size < frame * 2:
        raise SystemExit(f"only {size} bytes; a 1080p pair needs {frame * 2}")
    with open(raw, "rb") as handle:
        a = handle.read(frame)
        b = handle.read(frame)
    for name, data in (("frame_a.rgba", a), ("frame_b.rgba", b)):
        path = os.path.join(out, name)
        with open(path, "wb") as handle:
            handle.write(data)
        print(f"{path} {len(data)} bytes")


if __name__ == "__main__":
    main()
