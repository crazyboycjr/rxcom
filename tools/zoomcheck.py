#!/usr/bin/env python3
"""Checks a zoomed screenshot against the unzoomed one.

usage: zoomcheck.py UNZOOMED.png ZOOMED.png ZOOM POINTER_X POINTER_Y [nearest|linear]

The mapping is picom-zoom's: per axis, renderer_zoom_axis() picks a crop of the
screen and an integer origin, and screen pixel s shows the unzoomed screen at
q = crop + (s + 0.5 - origin) / zoom (pixel-centre coordinates), sampled with
the nearest pixel or bilinearly. Nearest must match exactly (either neighbour
where q falls on a pixel boundary); linear within 4 levels (pixman truncates
each axis's weight to 7 bits, 2 levels per axis on a full-contrast edge).
Exits 1 on a mismatch.
"""
import math
import sys

import numpy as np
from PIL import Image


def cround(v):
    """C's round(): halves away from zero."""
    return math.floor(v + 0.5) if v >= 0 else -math.floor(-v + 0.5)


def axis(size, scale, center):
    center = min(max(center, 0.5), size - 0.5)
    view_start = center * (1 - 1 / scale)
    crop_start = max(0, math.floor(view_start) - 1)
    crop_end = min(size, math.ceil(view_start + size / scale) + 1)
    origin = cround(crop_start * scale - center * (scale - 1))
    lowest = math.ceil(size - 0.5 - (crop_end - crop_start - 0.5) * scale)
    highest = math.floor(-0.5 * (scale - 1))
    return crop_start, int(max(min(origin, highest), lowest))


def main():
    u_path, z_path, z, px, py = sys.argv[1:6]
    filt = sys.argv[6] if len(sys.argv) > 6 else "nearest"
    z, px, py = float(z), int(px), int(py)
    u = np.asarray(Image.open(u_path).convert("RGB")).astype(np.float64)
    got = np.asarray(Image.open(z_path).convert("RGB")).astype(np.float64)
    h, w, _ = u.shape
    (cx, ox), (cy, oy) = axis(w, z, px + 0.5), axis(h, z, py + 0.5)
    qx = cx + (np.arange(w) + 0.5 - ox) / z
    qy = cy + (np.arange(h) + 0.5 - oy) / z
    if filt == "nearest":
        # Where q falls on a pixel boundary (every fifth pixel at 2.5x), either
        # neighbour is right: pixman takes the lower one, a GPU sampler the upper.
        def pick(d):
            return u[np.clip(np.floor(qy + d[1]).astype(int), 0, h - 1)][:, np.clip(np.floor(qx + d[0]).astype(int), 0, w - 1)]

        tie = 0.02
        cands = [pick((dx, dy)) for dx in (-tie, tie) for dy in (-tie, tie)]
        errs = [np.abs(c - got).max(axis=2) for c in cands]
        err = np.minimum.reduce(errs)
        want = None
        tolerance = 0
    else:
        sx, sy = qx - 0.5, qy - 0.5
        x0, y0 = np.floor(sx).astype(int), np.floor(sy).astype(int)
        fx, fy = (sx - x0)[None, :, None], (sy - y0)[:, None, None]
        cx0, cx1 = np.clip(x0, 0, w - 1), np.clip(x0 + 1, 0, w - 1)
        cy0, cy1 = np.clip(y0, 0, h - 1), np.clip(y0 + 1, 0, h - 1)
        top = u[cy0][:, cx0] * (1 - fx) + u[cy0][:, cx1] * fx
        bot = u[cy1][:, cx0] * (1 - fx) + u[cy1][:, cx1] * fx
        want = top * (1 - fy) + bot * fy
        tolerance = 4
        err = np.abs(want - got).max(axis=2)
    bad = err > tolerance
    print(
        f"zoom {z} {filt} pointer ({px},{py}) crop ({cx},{cy}) origin ({ox},{oy}): "
        f"max error {err.max():.2f}, pixels off by more than {tolerance}: {bad.sum()}"
    )
    if bad.any():
        ys, xs = np.nonzero(bad)
        print(f"bbox x {xs.min()}..{xs.max()} y {ys.min()}..{ys.max()}")
    print(f"under the cursor: unzoomed {u[py, px].astype(int)} zoomed {got[py, px].astype(int)}")
    sys.exit(1 if bad.any() else 0)


main()
