#!/usr/bin/env python3
"""Fetch FreeBSD packages from pkg.freebsd.org, checked end to end.

The repository's catalogue (packagesite.yaml) is signed by the FreeBSD
package key; this checks that signature against the key's pinned
fingerprint, then checks every package file against the catalogue's
checksum before it is used. Nothing here trusts TLS alone.

    pkgfetch.py --abi FreeBSD:16:amd64 --repo base_latest --out DIR \
        [--deps] [--match REGEX ...] [--exclude REGEX ...] [NAME ...]

Writes DIR/<name>.pkg and DIR/SOURCES (name, version, licenses, sha256 of
the file) and prints the names it fetched. --deps adds every package the
named ones depend on (run time dependencies, transitively).

Used by freebsd-sysroot.sh and build-c-dev-layer.sh (dev-tools-plan.md M3).
"""
import argparse, hashlib, io, json, os, re, subprocess, sys, tarfile, tempfile, urllib.request

MIRROR = "https://pkg.freebsd.org"
# sha256 of the PEM public key that signs pkg.freebsd.org's catalogues
# (/usr/share/keys/pkg/trusted/pkg.freebsd.org.2013102301).
PINNED = {"b0170035af3acc5f3f3ae1859dc717101b4e6c1d0a794ad554928ca0cbb2f438"}
ALPHA = "ybndrfg8ejkmcpqxot1uwisza345h769"


def get(url):
    with urllib.request.urlopen(url, timeout=300) as r:
        return r.read()


def b32(b):
    """pkg's base32: low bits first, its own alphabet."""
    r = rl = 0
    o = []
    for x in b:
        r |= x << rl
        rl += 8
        while rl >= 5:
            o.append(ALPHA[r & 31]); r >>= 5; rl -= 5
    if rl:
        o.append(ALPHA[r & 31])
    return "".join(o)


def sum_ok(data, s):
    if "$" in s:
        kind, want = s.split("$", 1)
        if kind == "2":
            return b32(hashlib.blake2b(data).digest()) == want
        if kind == "1":
            return hashlib.sha256(data).hexdigest() == want
        if kind == "0":
            return b32(hashlib.sha256(data).digest()) == want
        raise SystemExit(f"pkgfetch: unknown checksum type {kind!r}")
    return hashlib.sha256(data).hexdigest() == s


def catalogue(base):
    raw = get(f"{base}/packagesite.pkg")
    files = {}
    # packagesite.pkg is a tar (zstd or xz); bsdtar reads either.
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "site.pkg")
        open(p, "wb").write(raw)
        subprocess.run(["bsdtar", "-xf", p, "-C", d], check=True)
        for n in ("packagesite.yaml", "packagesite.yaml.sig", "packagesite.yaml.pub"):
            files[n] = open(os.path.join(d, n), "rb").read()
        if hashlib.sha256(files["packagesite.yaml.pub"]).hexdigest() not in PINNED:
            raise SystemExit("pkgfetch: catalogue signed by an unknown key")
        hexd = hashlib.sha256(files["packagesite.yaml"]).hexdigest().encode()
        open(os.path.join(d, "h"), "wb").write(hexd)
        v = subprocess.run(["openssl", "dgst", "-sha256", "-verify", os.path.join(d, "packagesite.yaml.pub"),
                            "-signature", os.path.join(d, "packagesite.yaml.sig"), os.path.join(d, "h")],
                           capture_output=True)
        if v.returncode != 0:
            raise SystemExit("pkgfetch: catalogue signature does not verify")
    pk = {}
    for line in files["packagesite.yaml"].decode().splitlines():
        if line.strip():
            d = json.loads(line)
            pk[d["name"]] = d
    return pk


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--abi", required=True)
    ap.add_argument("--repo", default="latest")
    ap.add_argument("--out", required=True)
    ap.add_argument("--deps", action="store_true")
    ap.add_argument("--match", action="append", default=[])
    ap.add_argument("--exclude", action="append", default=[])
    ap.add_argument("names", nargs="*")
    a = ap.parse_args()
    base = f"{MIRROR}/{a.abi}/{a.repo}"
    pk = catalogue(base)
    want = []
    for n in a.names:
        if n not in pk:
            raise SystemExit(f"pkgfetch: no package {n} in {base}")
        want.append(n)
    for rx in a.match:
        want += [n for n in sorted(pk) if re.search(rx, n) and n not in want]
    want = [n for n in want if not any(re.search(x, n) for x in a.exclude)]
    if a.deps:
        i = 0
        while i < len(want):
            for dep in sorted(pk[want[i]].get("deps", {})):
                if any(re.search(x, dep) for x in a.exclude):
                    continue  # left out on purpose, as dependency too
                if dep not in want:
                    if dep not in pk:
                        raise SystemExit(f"pkgfetch: {want[i]} needs {dep}, not in the catalogue")
                    want.append(dep)
            i += 1
    os.makedirs(a.out, exist_ok=True)
    rows = []
    for n in want:
        d = pk[n]
        dest = os.path.join(a.out, n + ".pkg")
        data = open(dest, "rb").read() if os.path.exists(dest) else None
        if data is None or not sum_ok(data, d["sum"]):
            data = get(f"{base}/{d['path'].lstrip('./')}")
            if not sum_ok(data, d["sum"]):
                raise SystemExit(f"pkgfetch: {n}: checksum does not match the signed catalogue")
            open(dest, "wb").write(data)
        rows.append(f"{n}\t{d['version']}\t{' '.join(d.get('licenses', []) or ['?'])}\t{d.get('licenselogic','single')}\t{hashlib.sha256(data).hexdigest()}")
        print(n)
    with open(os.path.join(a.out, "SOURCES"), "w") as f:
        f.write(f"# {base}\n# name\tversion\tlicenses\tlogic\tsha256\n")
        f.write("\n".join(rows) + "\n")


if __name__ == "__main__":
    main()
