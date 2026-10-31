#!/usr/bin/env python3
"""f202: objective comparison of the 画质增强 screenshots against `关闭`.

    python scripts/qa-f202-metrics.py --crop 0,60,1216,560 \\
        docs/f202_sr_off.png docs/f202_sr_cas.png docs/f202_sr_anime4k.png \\
        docs/f202_sr_fsr.png docs/f202_sr_rtxvsr.png

The first file is the reference (`关闭`). Every number is computed on the luma
channel of the same crop of every frame, because the captures are the same
paused picture: only the filter differs.

  * **PSNR** — dB, higher is "closer to 关闭". A sharpener moves away from the
    reference on purpose, so a *lower* PSNR is expected and not a defect.
  * **SSIM** — 11x11 box-windowed, C1/C2 from the standard formula; 1.0 is
    identical. Same reading as PSNR: this is a distance from 关闭, not a quality
    score on its own.
  * **gradient** — mean |∇luma| (Sobel) over the crop, i.e. sharpness / detail
    energy. *Higher* means more high-frequency content: this is the number that
    shows a sharpener actually sharpened.

Exit code 0 when every file loaded and every capture has the reference's size.
"""

import argparse
import sys

import numpy as np
from PIL import Image

C1 = (0.01 * 255) ** 2
C2 = (0.03 * 255) ** 2


def luma(path: str, crop):
    image = Image.open(path).convert("RGB")
    if crop:
        left, top, width, height = crop
        image = image.crop((left, top, left + width, top + height))
    pixels = np.asarray(image, dtype=np.float64)
    return 0.299 * pixels[:, :, 0] + 0.587 * pixels[:, :, 1] + 0.114 * pixels[:, :, 2]


def _window_means(values: np.ndarray, radius: int) -> np.ndarray:
    """Mean over a (2r+1)^2 window, edge-padded, via an integral image."""
    size = 2 * radius + 1
    padded = np.pad(values, radius, mode="edge")
    integral = np.zeros((padded.shape[0] + 1, padded.shape[1] + 1), dtype=np.float64)
    integral[1:, 1:] = padded.cumsum(axis=0).cumsum(axis=1)
    total = (
        integral[size:, size:]
        - integral[:-size, size:]
        - integral[size:, :-size]
        + integral[:-size, :-size]
    )
    return total / float(size * size)


def psnr(reference: np.ndarray, frame: np.ndarray) -> float:
    mse = np.mean((reference - frame) ** 2)
    if mse <= 0:
        return float("inf")
    return 10.0 * np.log10(255.0 * 255.0 / mse)


def ssim(reference: np.ndarray, frame: np.ndarray, radius: int = 5) -> float:
    mu_a = _window_means(reference, radius)
    mu_b = _window_means(frame, radius)
    aa = _window_means(reference * reference, radius)
    bb = _window_means(frame * frame, radius)
    ab = _window_means(reference * frame, radius)
    var_a = aa - mu_a * mu_a
    var_b = bb - mu_b * mu_b
    cov = ab - mu_a * mu_b
    numerator = (2 * mu_a * mu_b + C1) * (2 * cov + C2)
    denominator = (mu_a * mu_a + mu_b * mu_b + C1) * (var_a + var_b + C2)
    return float(np.mean(numerator / denominator))


def gradient(frame: np.ndarray) -> float:
    gx = np.zeros_like(frame)
    gy = np.zeros_like(frame)
    gx[:, 1:-1] = (frame[:, 2:] - frame[:, :-2]) * 0.5
    gy[1:-1, :] = (frame[2:, :] - frame[:-2, :]) * 0.5
    return float(np.mean(np.sqrt(gx * gx + gy * gy)))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("files", nargs="+")
    parser.add_argument(
        "--crop",
        default="",
        help="x,y,w,h in pixels; default: the whole capture",
    )
    parser.add_argument("--labels", default="", help="comma separated labels, same order")
    args = parser.parse_args()

    crop = None
    if args.crop:
        crop = tuple(int(part) for part in args.crop.split(","))
        if len(crop) != 4:
            print("--crop needs x,y,w,h", file=sys.stderr)
            return 2

    labels = args.labels.split(",") if args.labels else []
    frames = []
    for path in args.files:
        try:
            frames.append((path, luma(path, crop)))
        except OSError as error:
            print(f"{path}: {error}", file=sys.stderr)
            return 1
    reference = frames[0][1]
    for path, frame in frames[1:]:
        if frame.shape != reference.shape:
            print(f"{path}: size {frame.shape} != reference {reference.shape}", file=sys.stderr)
            return 1

    print(f"{'tier':<10} {'PSNR dB':>8} {'SSIM':>8} {'gradient':>9}")
    for index, (path, frame) in enumerate(frames):
        label = labels[index] if index < len(labels) else path.rsplit("/", 1)[-1]
        if index == 0:
            print(f"{label:<10} {'ref':>8} {'1.0000':>8} {gradient(frame):>9.3f}")
            continue
        print(
            f"{label:<10} {psnr(reference, frame):>8.2f} {ssim(reference, frame):>8.4f} "
            f"{gradient(frame):>9.3f}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
