#!/usr/bin/env python3
"""QA helper: cache one video's audio as 16 kHz mono WAV.

This mirrors `fishwatch::asr::ensure_audio` for manual acceptance runs where
starting the whole app is not wanted: same cache path (`cache/fishaudio/
<bvid>-<cid>.wav`), same ffmpeg normalisation. It is a developer tool, not part
of the product.

    python scripts/qa-audio.py BV1zaHv69EEr 42423487647
"""

import json
import os
import subprocess
import sys
import urllib.parse
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CACHE = os.path.join(ROOT, "cache", "fishaudio")

UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"


def cookies():
    with open(os.path.join(ROOT, "token.json"), encoding="utf-8") as handle:
        token = json.load(handle)
    return "; ".join(f"{c['name']}={c['value']}" for c in token["cookies"])


def get_json(url, cookie):
    request = urllib.request.Request(url, headers={"User-Agent": UA, "Cookie": cookie, "Referer": "https://www.bilibili.com/"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def audio_url(bvid, cid, cookie):
    query = urllib.parse.urlencode({"bvid": bvid, "cid": cid, "fnval": 16, "fourk": 0, "fnver": 0})
    data = get_json(f"https://api.bilibili.com/x/player/playurl?{query}", cookie)
    if data.get("code") != 0:
        raise SystemExit(f"playurl code={data.get('code')} {data.get('message')}")
    dash = data["data"].get("dash") or {}
    tracks = dash.get("audio") or []
    if not tracks:
        raise SystemExit("no dash audio track")
    # 30280 ≈ 192 kbit/s: what the app asks for when quality is "high".
    tracks.sort(key=lambda track: -track.get("bandwidth", 0))
    return tracks[0]["baseUrl"]


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    bvid, cid = sys.argv[1], sys.argv[2]
    os.makedirs(CACHE, exist_ok=True)
    cookie = cookies()
    wav = os.path.join(CACHE, f"{bvid}-{cid}.wav")
    if os.path.exists(wav) and os.path.getsize(wav) > 1000:
        print(f"cached {wav} bytes={os.path.getsize(wav)}")
        return
    url = audio_url(bvid, cid, cookie)
    m4a = os.path.join(CACHE, f"{bvid}-{cid}.m4a.part")
    # curl, not urllib: urllib's default headers get a 567 from the CDN, and the
    # CDN also answers a plain GET with a 567 anti-leech page — it wants a Range
    # request, which `-r 0-` provides.
    subprocess.run(
        [
            "curl", "-sS", "-L", "--retry", "3", "-r", "0-",
            "-A", UA,
            "-e", f"https://www.bilibili.com/video/{bvid}",
            "-o", m4a, url,
        ],
        check=True,
    )
    print(f"downloaded {os.path.getsize(m4a)} bytes")
    part = wav + ".part"
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-i", m4a, "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", "-f", "wav", part],
        check=True,
    )
    os.replace(part, wav)
    os.remove(m4a)
    print(f"wav {wav} bytes={os.path.getsize(wav)}")


if __name__ == "__main__":
    main()
