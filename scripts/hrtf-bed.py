#!/usr/bin/env python3
"""Build lyrad's HRTF bed (docs/spec/lyra-spatial.md S3) from the FABIAN database.

    python3 -I scripts/hrtf-bed.py <FABIAN_HRIR_measured_HATO_0.sofa> \
        <FABIAN_CTF_measured_inverted_smoothed.sofa> lyra/data/hrtf-bed-fabian-48k.bin

Source: the FABIAN head-related transfer function database (Brinkmann et al.,
TU Berlin, DOI 10.14279/depositonce-5718.5), CC BY 4.0, file
HRIRs_neutral_head_orientation_v4.zip. Changes made here, as CC BY asks us to
say: the eight 7.1 bed directions picked from the measured HRIRs (head-above-
torso orientation 0), diffuse-field equalised with FABIAN's own inverted,
smoothed common transfer function, resampled from 44.1 to 48 kHz, cut to 256
taps with a half-Hann fade, and scaled so a sound in both front speakers is as
loud as it was on plain stereo. The LFE row is a delayed impulse (no
direction) at the front's arrival time.

Needs numpy, scipy, h5py. Output, little-endian:
    b"LYHR", u32 version 1, u32 rate, u32 taps, u32 speakers (8), u32 ears (2),
    then f32 [speaker][ear][tap], speakers in 7.1 order FL FR FC LFE SL SR RL RR,
    ears left, right.
"""
import hashlib, struct, sys
import h5py, numpy as np
from scipy.signal import resample_poly

# The bed's azimuths (degrees, + = right, as lyra::binaural's BED_AZ); None = LFE.
BED_AZ = [-30, 30, 0, None, -110, 110, -150, 150]
RATE, TAPS = 48000, 256


def vec(az, el):
    a, e = np.radians(az), np.radians(el)
    return np.stack([np.cos(e) * np.cos(a), np.cos(e) * np.sin(a), np.sin(e)], -1)


def main(hrir_path, ctf_path, out_path):
    h = h5py.File(hrir_path, "r")
    assert h.attrs["DatabaseName"].startswith(b"FABIAN"), "not the FABIAN database"
    assert b"by/4.0" in h.attrs["License"], h.attrs["License"]
    ir, fs, pos = h["Data.IR"][()], float(h["Data.SamplingRate"][()][0]), h["SourcePosition"][()]
    ctf = h5py.File(ctf_path, "r")["Data.IR"][()][0, 0]
    v = vec(pos[:, 0], pos[:, 1])
    fade = np.hanning(64)[32:]
    bed = np.zeros((8, 2, TAPS))
    for s, az in enumerate(BED_AZ):
        if az is None:
            continue
        # SOFA azimuth is counter-clockwise (+ = left): ours is + = right.
        i = int(np.argmax(v @ vec(-az, 0)))
        assert abs(((pos[i, 0] + az + 180) % 360) - 180) < 1.5 and abs(pos[i, 1]) < 1.5, pos[i]
        for e in range(2):
            x = np.convolve(ir[i, e], ctf)
            x = resample_poly(x, 160, 147)[:TAPS]
            x[-32:] *= fade
            bed[s, e, : len(x)] = x
    # Loudness: the front pair, coherent, at each ear, averaged over 100 Hz to
    # 10 kHz on a log scale, is unity (stereo content stays as loud).
    n = 8192
    f = np.fft.rfftfreq(n, 1 / RATE)
    band = np.unique(np.round(np.geomspace(100, 10000, 200) / (RATE / n)).astype(int))
    p = [np.mean(np.abs(np.fft.rfft(bed[0, e] + bed[1, e], n))[band] ** 2) for e in range(2)]
    bed *= 1 / np.sqrt(np.mean(p))
    # LFE: no direction; arrives with the front, at the front centre's bass level.
    fc = np.abs(np.fft.rfft(bed[2], n, axis=1))[:, int(round(50 / (RATE / n)))].mean()
    onset = int(np.argmax(np.abs(bed[2, 0]) > 0.1 * np.abs(bed[2, 0]).max()))
    bed[3, :, onset] = fc
    with open(out_path, "wb") as o:
        o.write(b"LYHR" + struct.pack("<5I", 1, RATE, TAPS, 8, 2))
        o.write(bed.astype("<f4").tobytes())
    src = hashlib.sha256(open(hrir_path, "rb").read()).hexdigest()
    print(f"wrote {out_path}: 8 speakers x 2 ears x {TAPS} taps at {RATE} Hz; "
          f"scale {1 / np.sqrt(np.mean(p)):.4f}; LFE {fc:.3f} at tap {onset}; source sha256 {src}")


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    main(*sys.argv[1:])
