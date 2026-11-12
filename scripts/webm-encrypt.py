#!/usr/bin/env python3
"""Encrypt a WebM file for Clear Key tests (WebM Encryption, webmproject.org/docs/webm-encryption).

    webm-encrypt.py IN.webm OUT.webm KID_HEX KEY_HEX

Test tooling only. Every track gets a ContentEncryption (AES-CTR) with KID. Frames are
written in all three forms the spec allows, so a demuxer sees each:
  - video: partitioned (signal 0x03): a clear head, an encrypted middle, a clear tail;
    every 8th video frame is left clear (signal 0x00);
  - audio: whole frame encrypted (signal 0x01), the first frame left clear.
IVs are a counter starting at a fixed value, so the output is reproducible. Cues and
SeekHead are dropped (their offsets would be wrong); the player does not need them for
playback from the start.
"""
import sys
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

MASTER = {0x18538067, 0x1F43B675, 0xA0, 0x1654AE6B, 0xAE}  # Segment, Cluster, BlockGroup, Tracks, TrackEntry
DROP = {0x1C53BB6B, 0x114D9B74, 0xEC}  # Cues, SeekHead, Void


def vint(d, p):
    b = d[p]
    n = 1
    while n <= 8 and not b & (0x80 >> (n - 1)):
        n += 1
    v = b & ((0xFF >> n) if n < 8 else 0)
    for i in range(1, n):
        v = (v << 8) | d[p + i]
    return v, n


def read_id(d, p):
    b = d[p]
    n = 1
    while not b & (0x80 >> (n - 1)):
        n += 1
    return int.from_bytes(d[p:p + n], 'big'), n


def enc_id(i):
    return i.to_bytes((i.bit_length() + 7) // 8, 'big')


def enc_size(n):
    for l in range(1, 9):
        if n < (1 << (7 * l)) - 1:
            return ((1 << (7 * l)) | n).to_bytes(l, 'big')
    raise ValueError(n)


def el(i, body):
    return enc_id(i) + enc_size(len(body)) + body


def uint(i, v):
    return el(i, v.to_bytes(max(1, (v.bit_length() + 7) // 8), 'big'))


def parse(d, p, end):
    out = []
    while p < end:
        i, il = read_id(d, p)
        s, sl = vint(d, p + il)
        body = p + il + sl
        if s == (1 << (7 * sl)) - 1:  # unknown size
            s = end - body
        out.append((i, body, body + s))
        p = body + s
    return out


class Enc:
    def __init__(self, kid, key):
        self.kid, self.key = kid, key
        self.kinds = {}
        self.iv = 0x0102030405060708
        self.count = {}

    def ctr(self, iv, data):
        c = Cipher(algorithms.AES(self.key), modes.CTR(iv.to_bytes(8, 'big') + bytes(8))).encryptor()
        return c.update(data) + c.finalize()

    def block(self, d, s, e):
        tn, tl = vint(d, s)
        hdr = d[s:s + tl + 3]
        flags = d[s + tl + 2]
        assert (flags & 0x06) == 0, 'laced blocks are not supported'
        frame = bytes(d[s + tl + 3:e])
        video = self.kinds.get(tn) == 1
        n = self.count.get(tn, 0)
        self.count[tn] = n + 1
        if (video and n % 8 == 7) or (not video and n == 0):
            return hdr + b'\x00' + frame
        self.iv += 1
        iv = self.iv
        if video and len(frame) > 24:
            a, b = 10, len(frame) - 3
            body = frame[:a] + self.ctr(iv, frame[a:b]) + frame[b:]
            return hdr + b'\x03' + iv.to_bytes(8, 'big') + bytes([2]) + a.to_bytes(4, 'big') + b.to_bytes(4, 'big') + body
        return hdr + b'\x01' + iv.to_bytes(8, 'big') + self.ctr(iv, frame)

    def rewrite(self, d, s, e):
        out = b''
        for i, bs, be in parse(d, s, e):
            if i in DROP:
                continue
            if i == 0xAE:  # TrackEntry: note the type, add ContentEncodings
                kids = parse(d, bs, be)
                num = next(int.from_bytes(d[a:b], 'big') for j, a, b in kids if j == 0xD7)
                typ = next(int.from_bytes(d[a:b], 'big') for j, a, b in kids if j == 0x83)
                self.kinds[num] = typ
                aes = uint(0x47E8, 1)
                crypt = uint(0x47E1, 5) + el(0x47E2, self.kid) + el(0x47E7, aes)
                ce = uint(0x5031, 0) + uint(0x5032, 1) + uint(0x5033, 1) + el(0x5035, crypt)
                body = bytes(d[bs:be]) + el(0x6D80, el(0x6240, ce))
                out += el(i, body)
            elif i in MASTER:
                out += el(i, self.rewrite(d, bs, be))
            elif i in (0xA3, 0xA1):  # SimpleBlock, Block
                out += el(i, self.block(d, bs, be))
            else:
                out += el(i, bytes(d[bs:be]))
        return out


def main():
    src, dst, kid, key = sys.argv[1:5]
    d = open(src, 'rb').read()
    e = Enc(bytes.fromhex(kid), bytes.fromhex(key))
    out = b''
    for i, bs, be in parse(d, 0, len(d)):
        out += el(i, e.rewrite(d, bs, be) if i in MASTER else bytes(d[bs:be]))
    open(dst, 'wb').write(out)


if __name__ == '__main__':
    main()
