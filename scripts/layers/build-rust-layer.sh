#!/bin/sh
# The Rust layer (dev-tools-plan.md M3, dev-tools.md §5.1): rustc, cargo,
# std for both FreeBSD targets, rust-src, clippy, rustfmt, rust-analyzer,
# cross-built from Linux with x.py dist for the aarch64 and x86_64 FreeBSD
# hosts. Runs in a cloud thread (P2); about six hours on 4 cores.
#
#   sh scripts/layers/build-rust-layer.sh [VERSION]      # default 1.100.0
#
# Env: WORK (default ~/work/rust-layer), JOBS (default nproc),
#      SYSROOTS (default $WORK/sysroot, made with freebsd-sysroot.sh).
# Steps (each skipped when its output exists): source (sha256 + the Rust
# release key's signature), sysroots, x.py dist, then one layer tree per
# arch in $WORK/layer/rust-<version>-<arch>/ ready for `opifex layer pack`.
set -eu
VERSION=${1:-1.100.0}
WORK=${WORK:-$HOME/work/rust-layer}
JOBS=${JOBS:-$(nproc)}
HERE=$(cd "$(dirname "$0")" && pwd)
SYSROOTS=${SYSROOTS:-$WORK/sysroot}
LLVM_BIN=${LLVM_BIN:-/usr/lib/llvm-18/bin}
# The Rust release signing key (rust-key.gpg.ascii), by fingerprint.
RUST_KEY=108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE
say() { printf '== %s\n' "$*"; }
mkdir -p "$WORK"
cd "$WORK"

# 1. Source, checked.
SRC=rustc-$VERSION-src
if [ ! -d "$SRC" ]; then
    say "source $VERSION"
    curl -fsSO "https://static.rust-lang.org/dist/$SRC.tar.xz"
    curl -fsSO "https://static.rust-lang.org/dist/$SRC.tar.xz.sha256"
    curl -fsSO "https://static.rust-lang.org/dist/$SRC.tar.xz.asc"
    sha256sum -c "$SRC.tar.xz.sha256"
    if [ -n "${SKIP_GPG:-}" ]; then
        echo "WARNING: SKIP_GPG set, release signature not checked"
    else
        export GNUPGHOME="$WORK/gnupg"; mkdir -p "$GNUPGHOME"; chmod 700 "$GNUPGHOME"
        curl -fsS https://static.rust-lang.org/rust-key.gpg.ascii | gpg -q --import
        gpg -q --status-fd 1 --verify "$SRC.tar.xz.asc" "$SRC.tar.xz" | grep -q "VALIDSIG $RUST_KEY" \
            || { echo "build-rust-layer: $SRC.tar.xz is not signed by the Rust release key" >&2; exit 1; }
    fi
    tar xf "$SRC.tar.xz"
fi

# 2. Sysroots.
for a in amd64 aarch64; do
    [ -f "$SYSROOTS/$a/usr/lib/Scrt1.o" ] || sh "$HERE/freebsd-sysroot.sh" "$a" "$SYSROOTS/$a"
done

# 3. Cross compilers: clang for FreeBSD 16 against each sysroot.
TC="$WORK/toolchain"; mkdir -p "$TC"
for t in x86_64:amd64 aarch64:aarch64; do
    cpu=${t%%:*}; a=${t##*:}
    for kind in cc c++; do
        drv=clang; [ "$kind" = c++ ] && drv=clang++
        cat > "$TC/$cpu-freebsd-$kind" <<W
#!/bin/sh
exec $LLVM_BIN/$drv --target=$cpu-unknown-freebsd16 --sysroot=$SYSROOTS/$a -fuse-ld=lld "\$@"
W
        chmod +x "$TC/$cpu-freebsd-$kind"
    done
done

# 4. x.py dist.
cd "$SRC"
cat > bootstrap.toml <<T
change-id = "ignore"
profile = "dist"
[build]
build = "x86_64-unknown-linux-gnu"
host = ["x86_64-unknown-freebsd", "aarch64-unknown-freebsd"]
target = ["x86_64-unknown-freebsd", "aarch64-unknown-freebsd"]
extended = true
tools = ["cargo", "clippy", "rustfmt", "rust-analyzer", "rust-analyzer-proc-macro-srv", "src"]
docs = false
vendor = true
locked-deps = true
jobs = $JOBS
[llvm]
targets = "AArch64;X86;WebAssembly"
experimental-targets = ""
download-ci-llvm = false
ninja = true
[rust]
channel = "stable"
download-rustc = false
llvm-tools = false
[dist]
compression-formats = ["xz"]
compression-profile = "fast"
[target.x86_64-unknown-freebsd]
cc = "$TC/x86_64-freebsd-cc"
cxx = "$TC/x86_64-freebsd-c++"
linker = "$TC/x86_64-freebsd-cc"
ar = "$LLVM_BIN/llvm-ar"
ranlib = "$LLVM_BIN/llvm-ranlib"
[target.aarch64-unknown-freebsd]
cc = "$TC/aarch64-freebsd-cc"
cxx = "$TC/aarch64-freebsd-c++"
linker = "$TC/aarch64-freebsd-cc"
ar = "$LLVM_BIN/llvm-ar"
ranlib = "$LLVM_BIN/llvm-ranlib"
T
# cargo's openssl-sys and libz-sys link the base system's copies.
for t in X86_64:amd64 AARCH64:aarch64; do
    u=${t%%:*}; a=${t##*:}
    export "${u}_UNKNOWN_FREEBSD_OPENSSL_LIB_DIR=$SYSROOTS/$a/usr/lib"
    export "${u}_UNKNOWN_FREEBSD_OPENSSL_INCLUDE_DIR=$SYSROOTS/$a/usr/include"
done
export PKG_CONFIG_ALLOW_CROSS=1
if ! ls build/dist/rustc-$VERSION-aarch64-unknown-freebsd.tar.xz >/dev/null 2>&1; then
    say "x.py dist (long)"
    python3 x.py dist -j "$JOBS"
fi

# 5. One layer tree per arch: every component for that host, installed
#    under the layer's mount point, plus std for the other arch.
cd "$WORK"
for t in x86_64:amd64 aarch64:aarch64; do
    cpu=${t%%:*}; a=${t##*:}
    L="$WORK/layer/rust-$VERSION-$a"
    [ -f "$L/bin/rustc" ] && continue
    say "layer tree $L"
    rm -rf "$L" "$WORK/unpack"; mkdir -p "$L" "$WORK/unpack"
    for c in rustc cargo clippy rustfmt rust-analyzer rust-std-x86_64-unknown-freebsd rust-std-aarch64-unknown-freebsd rust-src; do
        case $c in rust-std-*|rust-src) f="$SRC/build/dist/$c-$VERSION.tar.xz"; [ "$c" = rust-src ] || f="$SRC/build/dist/rust-std-$VERSION-${c#rust-std-}.tar.xz" ;;
                   *) f="$SRC/build/dist/$c-$VERSION-$cpu-unknown-freebsd.tar.xz" ;; esac
        [ -f "$f" ] || { echo "build-rust-layer: missing $f" >&2; exit 1; }
        tar xf "$f" -C "$WORK/unpack"
        d="$WORK/unpack/$(basename "$f" .tar.xz)"
        # The installer writes to DESTDIR+prefix without running anything built.
        sh "$d/install.sh" --prefix=/ --destdir="$L" --disable-ldconfig --disable-verify >/dev/null
    done
    rm -rf "$L/lib/rustlib/uninstall.sh" "$L/lib/rustlib/install.log" "$L/share/doc" "$L/etc"
    cp "$SRC/LICENSE-MIT" "$SRC/LICENSE-APACHE" "$SRC/COPYRIGHT" "$L/"
    rm -rf "$WORK/unpack"
done
say "done: $WORK/layer"
