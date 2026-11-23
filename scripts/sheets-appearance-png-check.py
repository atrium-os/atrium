#!/usr/bin/env python3
"""Check the styled cell regions in the installed 1280 by 720 Sheets fixture."""
import sys
from pathlib import Path

from PIL import Image, ImageChops


def ink(image, col):
    left = 33 + 64 * col
    return [
        (x, y)
        for x in range(left, left + 58)
        for y in range(125, 209)
        if max(image.getpixel((x, y))) < 205
    ]


def check(directory, fmt):
    def load(state):
        return Image.open(directory / f"{fmt}-appearance-{state}.png").convert("RGB")

    before, after = load("initial"), load("reopened")
    assert before.size == after.size == (1280, 720), fmt
    # The only edit is in A1. Capture the styled labels, empty row, and a
    # normal blank row on each side of the band.
    region = (31, 123, 1088, 288)
    delta = ImageChops.difference(before.crop(region), after.crop(region))
    assert delta.getbbox() is None, (fmt, delta.getbbox())

    turned = ink(after, 1)
    tx, ty = zip(*turned)
    cx, cy = sum(tx) / len(tx), sum(ty) / len(ty)
    assert sum((x - cx) * (y - cy) for x, y in turned) / len(turned) < -20, fmt
    assert max(ty) - min(ty) > 20, fmt

    sx, sy = zip(*ink(after, 2))
    assert max(sx) - min(sx) >= 48 and max(sy) - min(sy) <= 9, fmt
    vx, vy = zip(*ink(after, 6))
    assert max(vx) - min(vx) <= 9 and max(vy) - min(vy) >= 58, fmt

    for left in (355, 384):
        for top in (125, 167):
            reds = [
                (r, g, b)
                for x in range(left, left + 28)
                for y in range(top, top + 39)
                for r, g, b in [after.getpixel((x, y))]
                if r > 180 and g < 70 and b < 70
            ]
            assert len(reds) >= 20, (fmt, left, top)

    paper = after.getpixel((511, 239))
    band = after.getpixel((511, 258))
    assert band != paper and after.getpixel((511, 279)) == paper, fmt
    if fmt != "ods":
        blue, gold = 0, 0
        for x in range(226, 285):
            for y in range(125, 209):
                r, g, b = after.getpixel((x, y))
                blue += b > 170 and r < 100
                gold += r > 220 and g > 180 and b < 110
        assert blue > 500 and gold > 500, fmt
    if fmt in {"xlsx", "xlsm"}:
        red = after.getpixel((291, 160))
        middle = after.getpixel((318, 160))
        green = after.getpixel((345, 160))
        assert red[0] > 185 and green[1] > 140 and max(middle) < 80, fmt
    print(f"PASS {fmt}: visible formatting and identical styled regions after reopen")


if __name__ == "__main__":
    for suffix in ("xlsx", "xlsm", "ods", "xls"):
        check(Path(sys.argv[1]), suffix)
