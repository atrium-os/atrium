p="/usr/src/stand/libsa/Makefile"
s=open(p).read()
seg=s.split("TESSERA read-only")[1].split(".include")[0]
if "extent.c" not in seg:
    s=s.replace("manifest.c pack.c volume.c","manifest.c pack.c volume.c extent.c",1)
    open(p,"w").write(s)
    print("added extent.c")
else:
    print("extent.c already present")
print([l.strip() for l in s.splitlines() if "tessera_reader.c" in l])
