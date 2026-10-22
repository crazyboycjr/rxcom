#!/usr/bin/env python3
"""Checks a zoomed screenshot against the unzoomed one.

usage: zoomcheck.py UNZOOMED.png ZOOMED.png ZOOM POINTER_X POINTER_Y

Screen pixel s must show the desktop at q = (s + 0.5) / z + o, with
o = p * (1 - 1/z), bilinearly filtered with clamp-to-edge, the mapping that
keeps the desktop point under the cursor fixed. Prints the error statistics
and exits 1 when any channel is off by more than 2.
"""
import sys

import numpy as np
from PIL import Image


def main():
    u_path, z_path, z, px, py = sys.argv[1:6]
    z, px, py = float(z), float(px), float(py)
    u = np.asarray(Image.open(u_path).convert("RGB")).astype(np.float64)
    got = np.asarray(Image.open(z_path).convert("RGB")).astype(np.float64)
    h, w, _ = u.shape
    ox, oy = px * (1 - 1 / z), py * (1 - 1 / z)
    sx = (np.arange(w) + 0.5) / z + ox - 0.5
    sy = (np.arange(h) + 0.5) / z + oy - 0.5
    x0 = np.floor(sx).astype(int)
    y0 = np.floor(sy).astype(int)
    fx = (sx - x0)[None, :, None]
    fy = (sy - y0)[:, None, None]
    cx0, cx1 = np.clip(x0, 0, w - 1), np.clip(x0 + 1, 0, w - 1)
    cy0, cy1 = np.clip(y0, 0, h - 1), np.clip(y0 + 1, 0, h - 1)
    top = u[cy0][:, cx0] * (1 - fx) + u[cy0][:, cx1] * fx
    bot = u[cy1][:, cx0] * (1 - fx) + u[cy1][:, cx1] * fx
    want = top * (1 - fy) + bot * fy
    err = np.abs(want - got).max(axis=2)
    print(
        f"zoom {z} pointer ({px:.0f},{py:.0f}) origin ({ox:.2f},{oy:.2f}): "
        f"max error {err.max():.2f}, mean {err.mean():.4f}, pixels off by >2: {(err > 2).sum()}"
    )
    ix, iy = int(px), int(py)
    print(f"under the cursor: unzoomed {u[iy, ix].astype(int)} zoomed {got[iy, ix].astype(int)}")
    sys.exit(1 if (err > 2).any() else 0)


main()
