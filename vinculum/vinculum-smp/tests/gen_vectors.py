# Regenerates tests/common/vectors.rs from Bumble's crypto (an independent
# implementation): python3 -I gen_vectors.py > common/vectors.rs
# with `pip install bumble` (0.0.236 was used).

import hashlib, bumble.crypto as c
def h(seed, n):
    out=b''; i=0
    while len(out)<n:
        out+=hashlib.sha256(f"{seed}/{i}".encode()).digest(); i+=1
    return out[:n]
def rs(b): return "hex!(\"%s\")" % b.hex()
print("// Generated from Bumble 0.0.236 bumble.crypto (independent implementation,")
print("// Apache-2.0) by vinculum-smp/tests/gen_vectors.py. Inputs and outputs")
print("// little-endian, as on air.")
print("use hex_literal::hex;")
print("pub struct V { pub name: &'static str, pub inputs: &'static [&'static [u8]], pub out: &'static [u8] }")
vs=[]
for i in range(4):
    k=h(f"cmac-k{i}",16); m=h(f"cmac-m{i}",[0,16,40,65][i])
    vs.append(("aes_cmac",[k,m],c.aes_cmac(m,k)))
for i in range(3):
    u=h(f"f4u{i}",32); v=h(f"f4v{i}",32); x=h(f"f4x{i}",16); z=bytes([[0,0x80,0x81][i]])
    vs.append(("f4",[u,v,x,z],c.f4(u,v,x,z)))
for i in range(3):
    w=h(f"f5w{i}",32); n1=h(f"f5n1{i}",16); n2=h(f"f5n2{i}",16); a1=h(f"f5a1{i}",7); a2=h(f"f5a2{i}",7)
    mk,ltk=c.f5(w,n1,n2,a1,a2)
    vs.append(("f5",[w,n1,n2,a1,a2],mk+ltk))
for i in range(3):
    w=h(f"f6w{i}",16); n1=h(f"f6n1{i}",16); n2=h(f"f6n2{i}",16); r=h(f"f6r{i}",16); io=h(f"f6io{i}",3); a1=h(f"f6a1{i}",7); a2=h(f"f6a2{i}",7)
    vs.append(("f6",[w,n1,n2,r,io,a1,a2],c.f6(w,n1,n2,r,io,a1,a2)))
for i in range(3):
    u=h(f"g2u{i}",32); v=h(f"g2v{i}",32); x=h(f"g2x{i}",16); y=h(f"g2y{i}",16)
    vs.append(("g2",[u,v,x,y],c.g2(u,v,x,y).to_bytes(4,'big')))
for i in range(2):
    k=h(f"ahk{i}",16); r=h(f"ahr{i}",3)
    vs.append(("ah",[k,r],c.ah(k,r)))
for i in range(2):
    da=(int.from_bytes(h(f"ecc-a{i}",32),'big') % (2**255)).to_bytes(32,'big'); db=(int.from_bytes(h(f"ecc-b{i}",32),'big') % (2**255)).to_bytes(32,'big')
    A=c.EccKey.from_private_key_bytes(da); B=c.EccKey.from_private_key_bytes(db)
    dh=A.dh(B.x,B.y)[::-1]
    vs.append(("ecdh",[da, B.x[::-1]+B.y[::-1]], dh))
    vs.append(("pubkey",[da], A.x[::-1]+A.y[::-1]))
print("pub const VECTORS: &[V] = &[")
for n,ins,out in vs:
    print(f"    V {{ name: \"{n}\", inputs: &[{', '.join('&'+rs(x) for x in ins)}], out: &{rs(out)} }},")
print("];")
