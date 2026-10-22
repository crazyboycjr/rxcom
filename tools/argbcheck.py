#!/usr/bin/env python3
"""Checks a premultiplied ARGB window's blend in a composited screenshot.

usage: argbcheck.py BELOW.png COMPOSED.png X,Y,W,H AARRGGBB

BELOW is the screen without the window; inside the rectangle every pixel must
be src + below * (1 - a), the premultiplied "over" operator, within 1 level.
"""
import sys

import numpy as np
from PIL import Image


def main():
    below_path, comp_path, rect, colour = sys.argv[1:5]
    x, y, w, h = map(int, rect.split(","))
    v = int(colour, 16)
    a, src = (v >> 24) & 0xFF, np.array([(v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF], float)
    below = np.asarray(Image.open(below_path).convert("RGB")).astype(float)[y : y + h, x : x + w]
    got = np.asarray(Image.open(comp_path).convert("RGB")).astype(float)[y : y + h, x : x + w]
    want = np.round(src + below * (255 - a) / 255)
    err = np.abs(want - got).max(axis=2)
    print(f"ARGB {colour} over {w}x{h}+{x}+{y}: max error {err.max():.0f}, pixels off by >1: {(err > 1).sum()}")
    sys.exit(1 if (err > 1).any() else 0)


main()
