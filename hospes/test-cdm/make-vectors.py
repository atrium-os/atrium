#!/usr/bin/env python3
"""Make the test CDM's vectors (docs/spec/hospes.md §11.3): vectors.bin and licence.json.

    make-vectors.py OUTDIR

Deterministic (fixed seed). Each record is one call the hosts make, with the clear bytes
the CDM must return. Record layout, little-endian:

    u8 op (1 decrypt, 2 decrypt-and-decode)   u8 scheme (0 clear, 1 cenc, 2 cbcs)
    u8 iv_len  u8 expect (cdm::Status)  u32 crypt  u32 skip  i64 timestamp
    kid[16]  iv[iv_len]  u32 nsub  nsub * (u32 clear, u32 cipher)
    u32 len  data[len]  u32 clear_len  clear[clear_len]
"""
import json, os, random, struct, sys, base64
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

KID1 = bytes.fromhex('00112233445566778899aabbccddeeff')
KEY1 = bytes.fromhex('3a2b1c0d4e5f60718293a4b5c6d7e8f9')
KID2 = bytes.fromhex('a0a1a2a3a4a5a6a7a8a9aaabacadaeaf')
KEY2 = bytes.fromhex('0f1e2d3c4b5a69788796a5b4c3d2e1f0')
KID3 = bytes.fromhex('deadbeefdeadbeefdeadbeefdeadbeef')  # never licensed
KEYS = {KID1: KEY1, KID2: KEY2}
rng = random.Random(20261112)


def rb(n):
    return bytes(rng.getrandbits(8) for _ in range(n))


def ctr(key, iv, data):
    c = Cipher(algorithms.AES(key), modes.CTR(iv + bytes(16 - len(iv)))).encryptor()
    return c.update(data) + c.finalize()


def ecb(key, block):
    c = Cipher(algorithms.AES(key), modes.ECB()).encryptor()
    return c.update(block) + c.finalize()


def enc_cenc(key, iv, clear, subs):
    # The cipher parts of all subsamples form one keystream.
    cipher_parts = b''.join(clear[o + c:o + c + e] for o, c, e in offsets(subs))
    ks = ctr(key, iv, cipher_parts)
    out, k = bytearray(clear), 0
    for o, c, e in offsets(subs):
        out[o + c:o + c + e] = ks[k:k + e]
        k += e
    return bytes(out)


def enc_cbcs(key, iv, clear, subs, crypt, skip):
    if crypt == 0 and skip == 0:
        crypt = 1
    out = bytearray(clear)
    for o, c, e in offsets(subs):
        p, left, chain = o + c, e, iv  # the IV restarts at every subsample
        while left:
            for _ in range(crypt):
                if left < 16:
                    break
                chain = ecb(key, bytes(a ^ b for a, b in zip(clear[p:p + 16], chain)))
                out[p:p + 16] = chain
                p += 16
                left -= 16
            if left < 16:
                break  # a trailing partial block stays clear
            s = min(skip * 16, left)
            p += s
            left -= s
    return bytes(out)


def offsets(subs):
    o = 0
    for c, e in subs:
        yield o, c, e
        o += c + e


recs = []


def rec(op, scheme, kid, iv, subs, data, clear, expect=0, crypt=0, skip=0, ts=None):
    ts = len(recs) * 41667 if ts is None else ts
    r = struct.pack('<BBBBIIq', op, scheme, len(iv), expect, crypt, skip, ts) + kid + iv
    r += struct.pack('<I', len(subs)) + b''.join(struct.pack('<II', c, e) for c, e in subs)
    r += struct.pack('<I', len(data)) + data + struct.pack('<I', len(clear)) + clear
    recs.append(r)


def cenc(kid, ivlen, subs, n=None, op=1, iv=None):
    n = sum(c + e for c, e in subs) if subs else n
    clear = rb(n)
    iv = iv or rb(ivlen)
    rec(op, 1, kid, iv, subs, enc_cenc(KEYS[kid], iv, clear, subs or [(0, n)]), clear)


def cbcs(kid, subs, crypt, skip, n=None, op=1):
    n = sum(c + e for c, e in subs) if subs else n
    clear = rb(n)
    iv = rb(16)
    rec(op, 2, kid, iv, subs, enc_cbcs(KEYS[kid], iv, clear, subs or [(0, n)], crypt, skip), clear,
        crypt=crypt, skip=skip)


# Decrypt only.
c = rb(100)
rec(1, 0, bytes(16), b'', [], c, c)                              # unencrypted
cenc(KID1, 8, [], 1000)                                          # whole sample, 8-byte IV
cenc(KID1, 16, [(10, 100), (0, 37), (5, 0), (3, 61)])            # subsamples share one keystream
cenc(KID1, 16, [(0, 64)], iv=bytes.fromhex('0001020304050607ffffffffffffffff'))  # counter carry
cenc(KID2, 8, [(16, 300)])                                       # the second key
cbcs(KID1, [(7, 500), (0, 333)], 1, 9)                           # pattern 1:9, partial tail, IV reset
cbcs(KID1, [], 0, 0, 999)                                        # no pattern: all whole blocks
cbcs(KID2, [(32, 1024), (4, 15)], 2, 8)                          # pattern 2:8, a sub-block subsample
c = rb(64)
rec(1, 1, KID3, rb(8), [], ctr(KEY1, rb(8), c), b'', expect=2)   # unknown key -> kNoKey
c = rb(64)
rec(1, 1, KID1, rb(8), [(10, 10)], c, b'', expect=4)             # map != size -> kDecryptError
# Decrypt and decode (the test decoder).
for i in range(3):
    cenc(KID1, 8, [(4, 200 + i * 50)], op=2)
cbcs(KID2, [(3, 300)], 1, 9, op=2)

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
with open(os.path.join(out, 'vectors.bin'), 'wb') as f:
    f.write(b''.join(recs))


def b64u(b):
    return base64.urlsafe_b64encode(b).rstrip(b'=').decode()


lic = {'keys': [{'kty': 'oct', 'kid': b64u(k), 'k': b64u(v)} for k, v in KEYS.items()], 'type': 'temporary'}
with open(os.path.join(out, 'licence.json'), 'w') as f:
    json.dump(lic, f, separators=(',', ':'))
print(f'{len(recs)} records', file=sys.stderr)
lic1 = {'keys': [{'kty': 'oct', 'kid': b64u(KID1), 'k': b64u(KEY1)}], 'type': 'persistent-license'}
with open(os.path.join(out, 'licence-kid1.json'), 'w') as f:
    json.dump(lic1, f, separators=(',', ':'))
