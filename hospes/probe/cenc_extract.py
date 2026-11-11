# Extract samples from a fragmented CENC MP4: writes samples.bin records for the CDM host
# and sps/pps for Annex-B output. Usage: cenc_extract.py in.mp4 outdir
import struct, sys, os
src, out = sys.argv[1], sys.argv[2]
d = open(src, 'rb').read()
def walk(b, off, end):
    while off + 8 <= end:
        sz, ty = struct.unpack('>I4s', b[off:off+8]); hdr = 8
        if sz == 1: sz = struct.unpack('>Q', b[off+8:off+16])[0]; hdr = 16
        if sz == 0: sz = end - off
        yield ty.decode('latin1'), off, off + hdr, off + sz
        off += sz
def find(b, off, end, path):
    for ty, o, s, e in walk(b, off, end):
        if ty == path[0]:
            if len(path) == 1: return (o, s, e)
            r = find(b, s + (8 if ty == 'stsd' else 0), e, path[1:])
            if r: return r
    return None
info = {}
stsd = find(d, 0, len(d), ['moov','trak','mdia','minf','stbl','stsd'])
entries = []
for ty, o, s, e in walk(d, stsd[1] + 8, stsd[2]):
    entries.append(ty)
    vs = s + 78
    for t2, o2, s2, e2 in walk(d, vs, e):
        if t2 == 'avcC':
            c = d[s2:e2]; info['avcc'] = c; info['nal_len'] = (c[4] & 3) + 1; p = 5; n = c[p] & 0x1f; p += 1; sps = []
            for _ in range(n): l = struct.unpack('>H', c[p:p+2])[0]; sps.append(c[p+2:p+2+l]); p += 2 + l
            n = c[p]; p += 1; pps = []
            for _ in range(n): l = struct.unpack('>H', c[p:p+2])[0]; pps.append(c[p+2:p+2+l]); p += 2 + l
            info['sps'], info['pps'] = sps, pps
        if t2 == 'sinf':
            for t3, o3, s3, e3 in walk(d, s2, e2):
                if t3 == 'schm': info['scheme'] = d[s3+4:s3+8].decode()
                if t3 == 'schi':
                    for t4, o4, s4, e4 in walk(d, s3, e3):
                        if t4 == 'tenc': x = d[s4:e4]; info['iv_size'] = x[7]; info['kid'] = x[8:24].hex()
print('sample entries', entries, 'scheme', info.get('scheme'), 'iv', info.get('iv_size'), 'kid', info.get('kid'), 'nal_len', info['nal_len'])
os.makedirs(out, exist_ok=True)
rec = open(os.path.join(out, 'samples.bin'), 'wb'); n_enc = n_clear = 0
for ty, o, s, e in walk(d, 0, len(d)):
    if ty == 'moof':
        moof = o; traf = find(d, s, e, ['traf']); sizes = []; senc = None; data_off = None
        for t2, o2, s2, e2 in walk(d, traf[1], traf[2]):
            if t2 == 'trun':
                fl = struct.unpack('>I', d[s2:s2+4])[0] & 0xffffff; cnt = struct.unpack('>I', d[s2+4:s2+8])[0]; p = s2 + 8
                if fl & 1: data_off = struct.unpack('>i', d[p:p+4])[0]; p += 4
                if fl & 4: p += 4
                for _ in range(cnt):
                    if fl & 0x100: p += 4
                    if fl & 0x200: sizes.append(struct.unpack('>I', d[p:p+4])[0]); p += 4
                    if fl & 0x400: p += 4
                    if fl & 0x800: p += 4
            if t2 == 'senc':
                fl = struct.unpack('>I', d[s2:s2+4])[0] & 0xffffff; cnt = struct.unpack('>I', d[s2+4:s2+8])[0]; p = s2 + 8; senc = []
                for _ in range(cnt):
                    iv = d[p:p+info['iv_size']]; p += info['iv_size']; subs = []
                    if fl & 2:
                        k = struct.unpack('>H', d[p:p+2])[0]; p += 2
                        for _ in range(k): subs.append(struct.unpack('>HI', d[p:p+6])); p += 6
                    senc.append((iv, subs))
        pos = moof + data_off
        for i, sz in enumerate(sizes):
            smp = d[pos:pos+sz]; pos += sz
            iv, subs = senc[i] if senc else (b'', [])
            rec.write(struct.pack('<IB', sz, 1 if senc else 0)); rec.write(smp)
            rec.write(struct.pack('<B', len(iv))); rec.write(iv.ljust(16, b'\0'))
            rec.write(struct.pack('<I', len(subs)))
            for c, x in subs: rec.write(struct.pack('<II', c, x))
            if senc: n_enc += 1
            else: n_clear += 1
rec.close()
open(os.path.join(out, 'avcc.bin'), 'wb').write(info['avcc'])
open(os.path.join(out, 'kid.hex'), 'w').write(info['kid'])
ps = b''.join(b'\0\0\0\1' + x for x in info['sps'] + info['pps'])
open(os.path.join(out, 'paramsets.h264'), 'wb').write(ps)
print('samples: encrypted', n_enc, 'clear', n_clear)
