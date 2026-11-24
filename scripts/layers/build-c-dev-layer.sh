#!/bin/sh
# The c-dev layer (dev-tools-plan.md M3, dev-tools.md §6.2): pkgconf, CMake,
# Meson, Ninja, GNU make, git, clangd and clang-format, lldb-dap, bear, Helix, from
# FreeBSD packages, relocated to /atrium/toolchains/c-dev. GPL tools are
# fine here: a layer is development tooling, never the base or a bundle.
#
#   sh scripts/layers/build-c-dev-layer.sh amd64|aarch64 [OUT]
#
# Env: VERSION (default 1), SIGN_KEY (PKCS#8 PEM; unsigned without it),
#      OPIFEX (default: cargo-built from this tree).
# Writes OUT/c-dev-<version>-<arch>/ (packed: atrium.toml, content.index,
# atrium.toml.sig when signed, LICENSES.md) and OUT/c-dev-<version>-<arch>.tar.xz.
set -eu
ARCH=${1:?amd64 or aarch64}
OUT=${2:-$HOME/work/c-dev-layer}
VERSION=${VERSION:-1}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
case "$ARCH" in amd64|aarch64) ;; *) echo "arch: amd64 or aarch64" >&2; exit 2 ;; esac
NAME=c-dev-$VERSION-$ARCH
PKGS="$OUT/pkgs-$ARCH"
STAGE="$OUT/stage-$ARCH"
L="$OUT/$NAME"
say() { printf '== %s\n' "$*"; }

# What the layer is for, and what is left out of it (perl: only git's
# send-email and svn need it; docs; the cmake manual).
WANT="pkgconf cmake meson ninja gmake git llvm21 bear helix"
say "fetch ($ARCH)"
python3 -I "$HERE/pkgfetch.py" --abi "FreeBSD:16:$ARCH" --repo latest --out "$PKGS" --deps \
    --exclude='^perl5$' --exclude='^p5-' --exclude='^cmake-doc$' --exclude='^cmake-man$' $WANT >/dev/null

say "extract"
rm -rf "$STAGE" "$L"; mkdir -p "$STAGE"
for n in $(grep -v '^#' "$PKGS/SOURCES" | cut -f1); do
    bsdtar -xf "$PKGS/$n.pkg" -C "$STAGE" --exclude '+*'
done
[ -d "$STAGE/usr/local" ] || { echo "no /usr/local in the packages" >&2; exit 1; }
others=$(cd "$STAGE" && find . -mindepth 1 -maxdepth 2 ! -path ./usr ! -path ./usr/local | head -5)
[ -z "$others" ] || echo "note: left out (outside /usr/local): $others"
mv "$STAGE/usr/local" "$L"
rm -rf "$STAGE"

say "trim"
cd "$L"
# clangd and clang-format, and lldb-dap (Artifex's debugger, dev-tools.md
# §7.3) with the lldb-server and lldb-argdumper it runs beside it, with
# what they load and clangd's headers.
if [ -d llvm21 ]; then
    find llvm21/bin -type f ! -name clangd ! -name clang-format ! -name lldb-dap ! -name lldb-server ! -name lldb-argdumper -delete
    find llvm21/bin -type l -delete
    rm -rf llvm21/include llvm21/share llvm21/libexec
    # What they load (RUNPATH $ORIGIN/../lib), the headers clangd parses
    # with, and lldb's Python module; nothing else of LLVM.
    find llvm21/lib -mindepth 1 -maxdepth 1 ! -name 'libLLVM*.so*' ! -name 'libclang-cpp.so*' ! -name 'liblldb.so*' ! -name clang ! -name 'python3*' -exec rm -rf {} +
    find llvm21/lib/clang -mindepth 2 -maxdepth 2 ! -name include -exec rm -rf {} +
    # The port's bin/*21 wrappers exec the binaries removed above.
    rm -f bin/*21
    # lldb-dap on PATH; its lldb-server stays beside it (the base's lldb
    # keeps the base's).
    for t in clangd clang-format lldb-dap; do ln -sf ../llvm21/bin/$t bin/$t; done
fi
rm -rf share/doc share/examples include libdata/pkgconfig share/aclocal share/info share/locale
# No perl in the layer (git's perl helpers go with it); Python without its
# test suite and GUI parts.
rm -rf lib/perl5
for d in test idlelib tkinter turtledemo ensurepip lib2to3; do rm -rf lib/python3*/$d; done
find lib/python3* -name __pycache__ -type d -prune -exec rm -rf {} + 2>/dev/null || true
find lib -maxdepth 1 -name '*.a' -delete 2>/dev/null || true
find . -name '*.la' -delete

say "relocate"
# Scripts: #!/usr/local/bin/x -> #!/usr/bin/env x (the layer's bin is first on PATH).
find bin libexec -type f 2>/dev/null | while read -r f; do
    if head -c 17 "$f" | grep -q '^#!/usr/local/bin/'; then
        sed -i '1s|^#!/usr/local/bin/|#!/usr/bin/env |' "$f"
    fi
done
# Links into /usr/local become relative; links out of the layer are dropped.
find . -type l | while read -r l; do
    t=$(readlink "$l")
    case "$t" in
        /usr/local/*)
            rel=$(python3 -I -c 'import os,sys; print(os.path.relpath(sys.argv[1], os.path.dirname(sys.argv[2])))' "${t#/usr/local/}" "${l#./}")
            ln -sfn "$rel" "$l" ;;
        /*) echo "note: dropped $l -> $t"; rm -f "$l" ;;
    esac
done
# Hard links (git's builtins) become links to the first name.
python3 -I - "$L" <<'PY'
import os, sys
top = sys.argv[1]; first = {}
for d, dirs, files in os.walk(top):
    dirs.sort(); files.sort()
    for f in files:
        p = os.path.join(d, f); st = os.lstat(p)
        if not os.path.isfile(p) or os.path.islink(p) or st.st_nlink < 2: continue
        k = (st.st_dev, st.st_ino)
        if k not in first: first[k] = p; continue
        os.unlink(p); os.symlink(os.path.relpath(first[k], d), p)
PY
n=$(find . -xtype l | wc -l); find . -xtype l -delete; [ "$n" = 0 ] || echo "note: $n dangling links removed"
chmod -R u+w,go-w . ; find . -type f -perm -4000 -exec chmod u-s {} + ; find . -type f -perm -2000 -exec chmod g-s {} +

say "licenses"
{
    echo "# c-dev $VERSION ($ARCH): what is in this layer"
    echo
    echo "From pkg.freebsd.org FreeBSD:16:$ARCH/latest, each package checked against the"
    echo "signed catalogue. GPL tools are allowed in development layers (dev-tools.md §2)."
    echo
    echo "| Package | Version | Licenses |"
    echo "|---|---|---|"
    grep -v '^#' "$PKGS/SOURCES" | awk -F'\t' '{printf "| %s | %s | %s (%s) |\n", $1, $2, $3, $4}'
} > LICENSES.md
cp "$PKGS/SOURCES" SOURCES

say "pack"
cat > "$OUT/$NAME.toml" <<T
kind = "layer"

[layer]
name = "c-dev"
version = "$VERSION"
arch = "$ARCH"
mount = "/atrium/toolchains/c-dev"
path = ["bin"]
summary = "C tools: pkgconf, CMake, Meson, Ninja, gmake, git, clangd, clang-format, lldb-dap, bear, Helix"

[layer.env]
LD_LIBRARY_PATH = "\${LAYER}/lib"
GIT_EXEC_PATH = "\${LAYER}/libexec/git-core"
GIT_TEMPLATE_DIR = "\${LAYER}/share/git-core/templates"
HELIX_RUNTIME = "\${LAYER}/share/helix/runtime"
T
OPIFEX=${OPIFEX:-}
if [ -z "$OPIFEX" ]; then
    (cd "$ROOT/portcullis" && cargo build -q -p opifex)
    OPIFEX="$ROOT/portcullis/target/debug/opifex"
    [ -x "$OPIFEX" ] || OPIFEX=$(find "$ROOT/portcullis/target/debug" -maxdepth 4 -name opifex -type f -perm -100 | head -1)
fi
"$OPIFEX" layer pack "$L" --manifest "$OUT/$NAME.toml" ${SIGN_KEY:+--sign "$SIGN_KEY"}
cd "$OUT"
tar -cJf "$NAME.tar.xz" "$NAME"
echo "c-dev: $L ($(du -sh "$L" | cut -f1)), $OUT/$NAME.tar.xz ($(du -h "$NAME.tar.xz" | cut -f1))"
