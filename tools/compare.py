#!/usr/bin/env python3
"""Compares two screenshots pixel by pixel, optionally ignoring rectangles or
comparing only within some.

usage: compare.py A.png B.png [--ignore X,Y,W,H]... [--only X,Y,W,H]...
Prints the number of differing pixels, the largest channel difference and the
bounding box of the differences; exits 1 when they differ.
"""
import sys

import numpy as np
from PIL import Image


def main():
    args = sys.argv[1:]
    paths = [a for a in args if not a.startswith("--") and "," not in a]
    rects = lambda flag: [tuple(map(int, args[i + 1].split(","))) for i, a in enumerate(args) if a == flag]
    a = np.asarray(Image.open(paths[0]).convert("RGB")).astype(int)
    b = np.asarray(Image.open(paths[1]).convert("RGB")).astype(int)
    d = np.abs(a - b).max(axis=2)
    if only := rects("--only"):
        keep = np.zeros_like(d, dtype=bool)
        for x, y, w, h in only:
            keep[y : y + h, x : x + w] = True
        d[~keep] = 0
    for x, y, w, h in rects("--ignore"):
        d[y : y + h, x : x + w] = 0
    n = int((d > 0).sum())
    print(f"differing pixels {n}, max channel difference {d.max()}")
    if n:
        ys, xs = np.nonzero(d)
        print(f"bbox x {xs.min()}..{xs.max()} y {ys.min()}..{ys.max()}")
    sys.exit(1 if n else 0)


main()
