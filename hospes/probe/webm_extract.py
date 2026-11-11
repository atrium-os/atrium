# Extract video frames from an encrypted WebM (WebM encryption: signal byte, 8-byte IV,
# optional partitions) into samples.bin for cdmhost. Usage: webm_extract.py in.webm outdir KIDHEX [MAXFRAMES]
import struct, sys, os
src, out, kid = sys.argv[1], sys.argv[2], sys.argv[3]; MAX = int(sys.argv[4]) if len(sys.argv) > 4 else 1 << 30
d = open(src, 'rb').read()
def vint(b, p, mask=True):
    x = b[p]; n = 1
    while n <= 8 and not (x & (0x80 >> (n - 1))): n += 1
    v = x & ((0x80 >> (n - 1)) - 1) if mask else x
    for i in range(1, n): v = (v << 8) | b[p + i]
    return v, n
def elems(b, p, end):
    while p < end:
        eid, n1 = vint(b, p, mask=False); sz, n2 = vint(b, p + n1)
        if sz == (1 << (7 * n2)) - 1: sz = end - p - n1 - n2
        yield eid, p + n1 + n2, p + n1 + n2 + sz; p = p + n1 + n2 + sz
os.makedirs(out, exist_ok=True); rec = open(os.path.join(out, 'samples.bin'), 'wb'); ne = nc = 0
def block(b, s, e):
    global ne, nc
    if ne + nc >= MAX: return
    tr, n = vint(b, s); p = s + n + 3; f = b[p:e]
    sig = f[0]; enc = sig & 1
    if enc:
        iv = f[1:9]; body = f[9:]; subs = []
        if sig & 2:
            np_ = f[9]; offs = [struct.unpack('>I', f[10 + 4*i:14 + 4*i])[0] for i in range(np_)]; body = f[10 + 4*np_:]
            bounds = [0] + offs + [len(body)]
            for i in range(0, len(bounds) - 1, 2):
                c = bounds[i+1] - bounds[i]; x = (bounds[i+2] - bounds[i+1]) if i + 2 < len(bounds) else 0; subs.append((c, x))
        else: subs = [(0, len(body))]
        ne += 1
    else: iv = b''; body = f[1:]; subs = []; nc += 1
    rec.write(struct.pack('<IB', len(body), enc)); rec.write(body); rec.write(struct.pack('<B', len(iv))); rec.write(iv.ljust(16, b'\0'))
    rec.write(struct.pack('<I', len(subs)))
    for c, x in subs: rec.write(struct.pack('<II', c, x))
for eid, s, e in elems(d, 0, len(d)):
    if eid == 0x18538067:
        for cid, cs, ce in elems(d, s, e):
            if cid == 0x1F43B675:
                for bid, bs, be in elems(d, cs, ce):
                    if bid == 0xA3: block(d, bs, be)
                    elif bid == 0xA0:
                        for xid, xs, xe in elems(d, bs, be):
                            if xid == 0xA1: block(d, xs, xe)
rec.close(); open(os.path.join(out, 'kid.hex'), 'w').write(kid); open(os.path.join(out, 'paramsets.h264'), 'wb').write(b'')
print('frames: encrypted', ne, 'clear', nc)
