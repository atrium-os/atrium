#!/usr/bin/env python3
"""Prelum R0 oracle helper: compare the pixels CUPS's ippeveps decoded from our
PWG Raster / URF (its PostScript output) with the test pages the oracle drew
(prelum-ipp-codec/examples/oracle.rs, test_page). Test-only.

usage: prelum-r0-pscheck.py <file.ps> <width> <height> <bytes-per-pixel> <pages>
"""
import base64, re, sys

def expected_row(w, h, bpp, page, y):
    row = bytearray(b"\xff" * (w * bpp))
    band = (y // 40) == page + 2
    for x in range(w):
        if x < 8 or y < 8 or x + 8 >= w or y + 8 >= h or (x * h) // w == y or band:
            for k in range(bpp):
                row[x * bpp + k] = k * 60
    return bytes(row)

def main():
    path, w, h, bpp, pages = sys.argv[1], *map(int, sys.argv[2:])
    data = open(path, "rb").read().decode("latin-1")
    page_texts = re.split(r"%%Page: ", data)[1:]
    if len(page_texts) != pages:
        sys.exit(f"FAIL {path}: {len(page_texts)} pages, expected {pages}")
    for p, text in enumerate(page_texts):
        rows = []
        for m in re.finditer(r"\n(\d+) L\n(.*?)~>", text, re.S):
            rows.append(base64.a85decode(m.group(2).replace("\n", "").encode(), adobe=False))
        if len(rows) != h:
            sys.exit(f"FAIL {path} page {p+1}: {len(rows)} rows, expected {h}")
        bad = [y for y in range(h) if rows[y] != expected_row(w, h, bpp, p, y)]
        if bad:
            sys.exit(f"FAIL {path} page {p+1}: {len(bad)} rows differ, first y={bad[0]}")
        print(f"ok   {path.split('/')[-1]} page {p+1}: {w}x{h}x{bpp} pixels identical")

main()
