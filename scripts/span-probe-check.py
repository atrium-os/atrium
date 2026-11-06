#!/usr/bin/env python3
"""Check monitor screendumps of atrium-span-probe spanning them (fresco-span.md S5).

Usage: span-probe-check.py OFFSETS DUMP.ppm [DUMP.ppm ...]

OFFSETS (comma-separated) are where the members' tiles start in the window
(window x), as the span event said: with three 1920 px monitors and a 40 px
bezel gap, 0,1960,3920. The dumps are of every pipe, in any order: a dump
that shows no probe (a pipe with no monitor) is skipped, and each tile must
be on exactly one. The probe paints 8 px columns whose colour encodes the column's
index across the whole window (atrium-test-client/src/span_probe.rs), so
each monitor must show columns OFFSET/8 onwards, in order: the picture runs
on across the members, skipping only the hidden bezel strips. It also checks
the tile frame the probe draws at each tile's left edge, and that frescod's
strip is not over the window (the probe's dark band shows at the top), as
when the members scan the window out directly.
Prints one line per monitor; exits 1 on any mismatch.
"""
import sys

STRIPE = 8


def read_ppm(path):
    with open(path, 'rb') as f:
        data = f.read()
    # P6 <w> <h> <max>\n<rgb...>, header fields separated by whitespace.
    fields, i = [], 0
    while len(fields) < 4:
        while data[i:i + 1].isspace():
            i += 1
        j = i
        while not data[j:j + 1].isspace():
            j += 1
        fields.append(data[i:j])
        i = j
    if fields[0] != b'P6':
        raise ValueError(f'{path}: not a P6 ppm')
    w, h = int(fields[1]), int(fields[2])
    return w, h, data[i + 1:]


def stripe_of(rgb):
    return (rgb[0] // 16) | (rgb[1] // 16) << 4 | (rgb[2] // 16) << 8


def main(args):
    offsets = [int(o) for o in args[0].split(',')]
    seen = []
    bad = 0
    for path in args[1:]:
        w, h, px = read_ppm(path)
        at = lambda x, y: tuple(px[(y * w + x) * 3:(y * w + x) * 3 + 3])
        row = h // 2
        # Which tile: the offset whose first stripe this dump starts with.
        first = stripe_of(at(STRIPE // 2, row))
        probe = all(2 <= v % 16 <= 14 for v in at(STRIPE // 2, row))
        off = next((o for o in offsets if o // STRIPE == first), None) if probe else None
        if w < 64 or off is None:
            print(f'{path}: {w}x{h}, no tile (first stripe {first}); skipped')
            continue
        seen.append(off)
        wrong = [c for c in range(STRIPE // 2, w, STRIPE) if stripe_of(at(c, row)) != (off + c) // STRIPE]
        frame = at(1, 30)
        band = at(w // 2, 30)
        top = at(w // 2, 8)
        notes = []
        if wrong:
            notes.append(f'{len(wrong)} columns wrong, first at x={wrong[0]} shows stripe {stripe_of(at(wrong[0], row))}, want {(off + wrong[0]) // STRIPE}')
        if min(frame) < 200:
            notes.append(f'no tile frame at its left edge ({frame})')
        if max(band) > 60 or max(top) > 60:
            notes.append(f'something over the top band ({top}): the strip?')
        print(f'{path}: tile at {off}: ' + ('; '.join(notes) if notes else f'continuous, stripes {off // STRIPE}..{(off + w - 1) // STRIPE}, frame at its edge, nothing over it'))
        bad += bool(notes)
    if sorted(seen) != sorted(offsets):
        print(f'tiles shown at {sorted(seen)}, want {sorted(offsets)}')
        bad += 1
    return 1 if bad else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
