#!/usr/bin/env python3
"""Judge a recording of lyra-play's counter signal (LYRA_PLAY_SIGNAL=counter).

Frame n of the signal is the 16-bit word n mod 65536 in the left channel and
its complement in the right. A path that carries every sample exactly gives a
run of frames whose left word goes up by one each frame. This counts what went
wrong between the first and the last counter frame:

  dropped   frames the count skipped
  repeated  frames whose count did not advance (or went back)
  corrupt   frames whose right word is not the left word's complement
  silent    all-zero frames inside the run (a gap)

usage: audio-counter-check.py FILE.wav [--max-dropped N] ... (all default 0)
Exits 1 when any count is over its limit, 2 when the file holds no counter.
Input is 16-bit stereo PCM WAV, raw S16LE stereo (--raw), or a wav still
being written read from a byte offset (--offset, 4-byte aligned).
"""
import argparse
import struct
import sys
import wave


def frames(path, raw, offset):
    if raw or offset:
        # A wav qemu is still writing has a header with zero sizes (it fills
        # them in at exit), so it is read as raw S16LE after the 44-byte
        # header, from `offset` (the file's size before the run).
        with open(path, "rb") as f:
            f.seek(max(offset, 0 if raw else 44))
            data = f.read()
        print(f"  {path}: {len(data) // 4} frames from byte {max(offset, 0 if raw else 44)}")
    else:
        with wave.open(path, "rb") as w:
            if w.getsampwidth() != 2 or w.getnchannels() != 2:
                sys.exit(f"{path}: want 16-bit stereo, got {w.getsampwidth() * 8}-bit {w.getnchannels()} ch")
            print(f"  {path}: {w.getframerate()} Hz, {w.getnframes()} frames")
            data = w.readframes(w.getnframes())
    n = len(data) // 4
    return struct.unpack(f"<{2 * n}h", data[: 4 * n])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--raw", action="store_true")
    ap.add_argument("--offset", type=int, default=0, help="byte offset to start at (a growing wav)")
    for k in ("dropped", "repeated", "corrupt", "silent"):
        ap.add_argument(f"--max-{k}", type=int, default=0)
    a = ap.parse_args()
    s = frames(a.file, a.raw, a.offset)
    lr = list(zip(s[0::2], s[1::2]))

    def is_counter(l, r):
        return (l ^ r) & 0xFFFF == 0xFFFF

    idx = [i for i, (l, r) in enumerate(lr) if is_counter(l, r) and (l, r) != (0, 0)]
    if len(idx) < 2:
        print("  no counter signal in the recording")
        sys.exit(2)
    first, last = idx[0], idx[-1]
    dropped = repeated = corrupt = silent = 0
    first_bad = []
    prev = None
    for i in range(first, last + 1):
        l, r = lr[i]
        if (l, r) == (0, 0):
            silent += 1
            continue
        if not is_counter(l, r):
            corrupt += 1
            if len(first_bad) < 5:
                first_bad.append(f"frame {i}: {l} {r} (corrupt)")
            continue
        c = l & 0xFFFF
        if prev is not None:
            step = (c - prev) & 0xFFFF
            if step == 0 or step > 0x8000:
                repeated += 1
                if len(first_bad) < 5:
                    first_bad.append(f"frame {i}: count {prev} -> {c} (repeat)")
            elif step > 1:
                dropped += step - 1
                if len(first_bad) < 5:
                    first_bad.append(f"frame {i}: count {prev} -> {c} (dropped {step - 1})")
        prev = c
    run = last - first + 1
    print(f"  counter run: {run} frames (from frame {first})")
    print(f"  dropped {dropped}, repeated {repeated}, corrupt {corrupt}, silent {silent}")
    for b in first_bad:
        print("   ", b)
    over = [k for k, v in (("dropped", dropped), ("repeated", repeated), ("corrupt", corrupt), ("silent", silent))
            if v > getattr(a, f"max_{k}")]
    if over:
        print(f"  FAIL: {', '.join(over)} over the limit")
        sys.exit(1)
    print("  ok: every frame carried exactly")


if __name__ == "__main__":
    main()
