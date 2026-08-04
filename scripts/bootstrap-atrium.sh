#!/bin/sh
#
# bootstrap-atrium.sh — clone, build and stage Atrium from nothing.
#
# For someone who has just been handed this repo and a Mac. It fetches every
# other repo Atrium needs, downloads FreeBSD, builds the patched QEMU, the
# patched FreeBSD kernel, the Tessera filesystem module and the whole Rust
# userspace, and stages the result under dist/.
#
#   sh scripts/bootstrap-atrium.sh              # run everything that is not done
#   sh scripts/bootstrap-atrium.sh --list       # show the phases and their state
#   sh scripts/bootstrap-atrium.sh --only kernel
#   sh scripts/bootstrap-atrium.sh --from qemu  # this phase and everything after
#   sh scripts/bootstrap-atrium.sh --force ...  # redo even if already marked done
#
# It does NOT boot anything. When it finishes you have artifacts staged in
# dist/ and a printed summary of what to do next. Booting is scripts/run-vm.sh.
#
# DESIGN NOTES, because this script will keep growing:
#
#   * Every phase is idempotent and leaves a stamp in .bootstrap-state/. Re-running
#     skips finished work, so a laptop that slept through the QEMU build can just
#     be re-run.
#   * Every phase VERIFIES its own output — a file exists, a symbol is present, a
#     binary is the right architecture. A phase that cannot prove it worked fails.
#     This project has repeatedly been bitten by builds that exited 0 having done
#     nothing (a sync that silently no-op'd, a .ko linked before its .o compiled),
#     so "make exited 0" is never accepted as evidence on its own.
#   * Errors say what to do about them, because the reader may not know FreeBSD.
#
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
STATE="${ATRIUM_STATE:-$REPO/.bootstrap-state}"
DIST="${ATRIUM_DIST:-$REPO/dist}"
LOGS="$STATE/logs"
mkdir -p "$STATE" "$LOGS"

# FreeBSD to track. CURRENT, not a release: Atrium needs 16.0-CURRENT.
FBSD_VER="${ATRIUM_FBSD_VER:-16.0-CURRENT}"
FBSD_ARCH="arm64/aarch64"
FBSD_MIRROR="${ATRIUM_FBSD_MIRROR:-https://download.freebsd.org/snapshots/${FBSD_ARCH}/${FBSD_VER}}"

# ---------------------------------------------------------------------------
# EVERY input location can be pointed somewhere else. A developer who already
# has these checked out (or a CI box with a shared cache) should never wait for
# a re-clone or a re-download:
#
#   ATRIUM_FBSD_SRC   existing freebsd-src checkout (must be on atrium-os)
#   ATRIUM_QEMU_DIR   existing Atrium-patched qemu tree
#   ATRIUM_QEMU_BIN   an already-built qemu-system-aarch64; skips the qemu build
#   ATRIUM_TARBALLS   directory already holding base/kernel/src .txz
#   ATRIUM_SYSROOT    an already-extracted FreeBSD sysroot
#   ATRIUM_OBJDIR     kernel build objdir (large; worth putting on a fast disk)
#   ATRIUM_STATE      where phase stamps and logs live
#   ATRIUM_DIST       where staged artifacts land
#
# Anything already present at the default path is reused too — the override
# exists for when it lives somewhere else entirely.
# ---------------------------------------------------------------------------
TARBALLS="${ATRIUM_TARBALLS:-$REPO/tarballs}"
SYSROOT="${ATRIUM_SYSROOT:-$REPO/sysroot}"
FBSD_SRC="${ATRIUM_FBSD_SRC:-$REPO/freebsd-src/usr/src}"
QEMU_DIR="${ATRIUM_QEMU_DIR:-$REPO/external/qemu-build}"
OBJDIR="${ATRIUM_OBJDIR:-$STATE/fbsdobj}"

TARGET_TRIPLE="aarch64-unknown-freebsd"
JOBS=$(sysctl -n hw.ncpu 2>/dev/null || echo 4)

# ---------------------------------------------------------------- output ----
if [ -t 1 ]; then
    B=$(printf '\033[1m'); R=$(printf '\033[0m')
    GRN=$(printf '\033[32m'); RED=$(printf '\033[31m'); YEL=$(printf '\033[33m')
else
    B=''; R=''; GRN=''; RED=''; YEL=''
fi
say()  { printf '%s\n' "$*"; }
head_() { printf '\n%s== %s ==%s\n' "$B" "$*" "$R"; }
ok()   { printf '  %sok%s   %s\n' "$GRN" "$R" "$*"; }
warn() { printf '  %swarn%s %s\n' "$YEL" "$R" "$*"; }
die()  { printf '\n%sFAILED%s %s\n' "$RED" "$R" "$1" >&2; shift
         [ $# -gt 0 ] && { printf '\n%s\n' "$*" >&2; }; exit 1; }

# ------------------------------------------------------------ phase plumbing --
PHASES="preflight clone fetch sysroot qemu kernel kmod userspace stage"

phase_desc() {
    case $1 in
    preflight) echo "check the host has what it needs";;
    clone)     echo "clone freebsd-src (atrium-os) and the patched QEMU";;
    fetch)     echo "download the FreeBSD $FBSD_VER distribution sets";;
    sysroot)   echo "extract base.txz into sysroot/ for cross-compiling";;
    qemu)      echo "build the Atrium-patched qemu-system-aarch64";;
    kernel)    echo "cross-build the FreeBSD kernel on this Mac";;
    kmod)      echo "cross-build the Tessera filesystem module";;
    userspace) echo "cross-build the Rust userspace (frescod, portcullis, forum apps)";;
    stage)     echo "assemble everything under dist/";;
    esac
}
done_stamp() { echo "$STATE/$1.done"; }
is_done()    { [ -f "$(done_stamp "$1")" ]; }
mark_done()  {
    # Shell functions have no local scope, so a phase body that reuses the outer
    # loop's variable name silently corrupts this. Refuse anything that is not a
    # known phase rather than writing a stamp with a path in its name.
    case " $PHASES " in *" $1 "*) ;; *) die "internal: mark_done '$1' is not a phase" \
        "A phase function clobbered the loop variable. Rename its locals.";; esac
    date '+%Y-%m-%dT%H:%M:%S' > "$(done_stamp "$1")"
}

usage() {
    say "Usage: sh scripts/bootstrap-atrium.sh [--list] [--only <phase>] [--from <phase>] [--force]"
    say ""
    say "Phases, in order:"
    for p in $PHASES; do printf '  %-10s %s\n' "$p" "$(phase_desc "$p")"; done
    say ""
    say "State lives in .bootstrap-state/ ; per-phase logs in .bootstrap-state/logs/."
}

cmd_list() {
    say "Phase state:"
    for p in $PHASES; do
        if is_done "$p"; then s="${GRN}done${R}  $(cat "$(done_stamp "$p")")"
        else s="${YEL}todo${R}"; fi
        printf '  %-10s %b  %s\n' "$p" "$s" "$(phase_desc "$p")"
    done
}

# ------------------------------------------------------------------ phases ---

ph_preflight() {
    [ "$(uname -s)" = "Darwin" ] || die "this script targets macOS" \
        "Atrium's VM runs under Hypervisor.framework, which is macOS-only."
    [ "$(uname -m)" = "arm64" ] || die "this script needs an Apple Silicon Mac" \
        "The guest is aarch64 and runs under HVF; an Intel Mac cannot host it."
    ok "macOS on Apple Silicon"

    command -v brew >/dev/null 2>&1 || die "Homebrew is not installed" \
        "Install it first — see https://brew.sh — then re-run this script."
    ok "homebrew present"

    # Xcode command line tools supply the host compiler and, importantly, headers.
    xcode-select -p >/dev/null 2>&1 || die "Xcode command line tools are missing" \
        "Run:  xcode-select --install
Then re-run this script."
    ok "xcode command line tools"

    missing=''
    for f in llvm bmake python3 git; do
        brew --prefix "$f" >/dev/null 2>&1 || missing="$missing $f"
    done
    if [ -n "$missing" ]; then
        die "missing Homebrew packages:$missing" \
"Install them with:
    brew install$missing
Then re-run this script.

(llvm is the cross compiler FreeBSD's build driver looks for; bmake is
FreeBSD's make; both must come from Homebrew, not Xcode.)"
    fi
    ok "llvm, bmake, python3, git"

    command -v rustup >/dev/null 2>&1 || die "rustup is not installed" \
"Install Rust with:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
Then re-run this script."
    # The tree pins nightly + rust-src in rust-toolchain.toml; rustup honours it
    # automatically, but rust-src must actually be fetched or build-std fails.
    rustup component add rust-src --toolchain nightly >/dev/null 2>&1 || true
    ok "rustup (tree pins nightly + rust-src)"

    # Disk. The kernel objdir, QEMU build and FreeBSD sets are large.
    avail=$(df -g "$REPO" | awk 'NR==2 {print $4}')
    if [ "${avail:-0}" -lt 40 ]; then
        die "only ${avail}G free on the volume holding $REPO" \
            "Budget at least 40G: FreeBSD sets ~1.5G, sysroot ~1G, kernel objdir ~6G,
QEMU build ~3G, Rust target dirs ~10G, and the VM disk image later on."
    fi
    ok "${avail}G free disk"
}

ph_clone() {
    # freebsd-src: the ATRIUM fork, atrium-os branch. That branch carries the
    # Laminar scheduler and the Tessera root hooks — a stock FreeBSD checkout
    # will build but will not be Atrium.
    if [ -d "$FBSD_SRC/.git" ]; then
        ok "reusing freebsd-src at $FBSD_SRC"
    else
        [ -n "${ATRIUM_FBSD_SRC:-}" ] && die "ATRIUM_FBSD_SRC=$FBSD_SRC is not a git checkout" \
            "Point it at an existing freebsd-src working tree, or unset it to clone one."
        head_ "cloning freebsd-src (this is large, ~10 min)"
        mkdir -p "$REPO/freebsd-src/usr"
        git clone --branch atrium-os --filter=blob:none \
            https://github.com/atrium-os/freebsd-src.git "$FBSD_SRC" \
            > "$LOGS/clone-freebsd.log" 2>&1 \
            || die "cloning freebsd-src failed" "See $LOGS/clone-freebsd.log"
    fi
    br=$(git -C "$FBSD_SRC" branch --show-current 2>/dev/null)
    [ "$br" = "atrium-os" ] || die "freebsd-src is on branch '$br', expected 'atrium-os'" \
"The atrium-os branch is what makes this Atrium rather than stock FreeBSD.
    git -C $FBSD_SRC checkout atrium-os"
    ok "freebsd-src on atrium-os"

    if [ -n "${ATRIUM_QEMU_BIN:-}" ]; then
        ok "ATRIUM_QEMU_BIN set — the qemu phase will be skipped entirely"
    elif [ -d "$QEMU_DIR/.git" ]; then
        ok "reusing qemu-build at $QEMU_DIR"
    else
        head_ "cloning the Atrium-patched QEMU (large, ~5 min)"
        mkdir -p "$(dirname "$QEMU_DIR")"
        git clone https://github.com/atrium-os/qemu.git "$QEMU_DIR" \
            > "$LOGS/clone-qemu.log" 2>&1 \
            || die "cloning qemu failed" "See $LOGS/clone-qemu.log"
        ok "qemu-build cloned"
    fi
}

ph_fetch() {
    mkdir -p "$TARBALLS"
    for set in base kernel src; do
        f="$TARBALLS/$set.txz"
        if [ -s "$f" ]; then ok "reusing $set.txz in $TARBALLS"; continue; fi
        head_ "downloading $set.txz from $FBSD_MIRROR"
        curl -fL --progress-bar -o "$f.part" "$FBSD_MIRROR/$set.txz" \
            || die "downloading $set.txz failed" \
"Check the mirror is reachable:  $FBSD_MIRROR/$set.txz
Snapshot builds are rotated, so an old URL can 404 — if so, browse
$FBSD_MIRROR and update FBSD_VER at the top of this script."
        mv "$f.part" "$f"
        ok "$set.txz"
    done
    # Prove they are really xz archives and not an HTML error page saved as .txz.
    for set in base kernel src; do
        tar -tJf "$TARBALLS/$set.txz" >/dev/null 2>&1 \
            || die "$TARBALLS/$set.txz is not a valid xz archive" \
                   "Delete it and re-run; the download was probably truncated or a 404 page."
    done
    ok "all three sets verified as xz archives"
}

ph_sysroot() {
    mkdir -p "$SYSROOT"
    # An already-populated sysroot (ours, or one pointed at by ATRIUM_SYSROOT) is
    # left alone — re-extracting would clobber local additions for no gain.
    if [ -e "$SYSROOT/lib/libc.so.7" ] && [ -e "$SYSROOT/libexec/ld-elf.so.1" ] \
       && [ -z "${FORCE:-}" ]; then
        ok "reusing populated sysroot at $SYSROOT"
        return 0
    fi
    # Headers + libs are what cross-compiling needs; the rtld is what opifex
    # needs to stage into app bundles. Extracting all of base would work but is
    # several GB of userland we never link against.
    head_ "extracting headers, libraries and the rtld into sysroot/"
    tar -xf "$TARBALLS/base.txz" -C "$SYSROOT" \
        ./usr/include ./usr/lib ./lib ./libexec/ld-elf.so.1 \
        > "$LOGS/sysroot.log" 2>&1 \
        || die "extracting base.txz failed" "See $LOGS/sysroot.log"

    # Verify against the things that actually get linked and staged.
    for f in usr/lib/libc.a lib/libc.so.7 libexec/ld-elf.so.1 usr/include/stdio.h; do
        [ -e "$SYSROOT/$f" ] || die "sysroot is missing $f" \
            "The extract did not produce a usable sysroot. Remove sysroot/ and re-run."
    done
    ok "sysroot has libc, the rtld and headers"

    # freebsd-src also needs to exist for kmod headers; the clone phase did that.
    [ -d "$FBSD_SRC/sys" ] || die "freebsd-src/usr/src/sys is missing" \
        "Re-run the clone phase:  sh scripts/bootstrap-atrium.sh --only clone"
    ok "kernel sources present"
}

ph_qemu() {
    if [ -n "${ATRIUM_QEMU_BIN:-}" ]; then
        [ -x "$ATRIUM_QEMU_BIN" ] || die "ATRIUM_QEMU_BIN=$ATRIUM_QEMU_BIN is not executable" \
            "Point it at a built qemu-system-aarch64, or unset it to build one."
        ok "using prebuilt qemu: $ATRIUM_QEMU_BIN"
        return 0
    fi
    q="$QEMU_DIR"
    bin="$q/build/qemu-system-aarch64"
    if [ -x "$bin" ] && [ -z "${FORCE:-}" ]; then
        ok "qemu already built ($bin)"
        return 0
    fi
    head_ "building QEMU — this is the long one, 20-45 minutes"
    say "  (a full log is at $LOGS/qemu.log)"
    ( cd "$q" && ./configure \
          --target-list=aarch64-softmmu \
          --enable-hvf --enable-cocoa \
          --disable-docs --disable-werror ) > "$LOGS/qemu.log" 2>&1 \
        || die "qemu configure failed" \
"See $LOGS/qemu.log. Missing build dependencies are the usual cause:
    brew install pkg-config glib pixman ninja meson"
    ( cd "$q" && make -j"$JOBS" ) >> "$LOGS/qemu.log" 2>&1 \
        || die "qemu build failed" "See $LOGS/qemu.log"
    [ -x "$bin" ] || die "qemu build reported success but $bin does not exist" \
        "See $LOGS/qemu.log"
    ok "qemu-system-aarch64 built"
}

# make.py bootstraps its own bmake, and that bootstrap runs bmake's unit tests.
# Two of them (cmd-interrupt, deptgt-interrupt) fail on macOS because Darwin does
# not deliver SIGINT the way they expect. The bmake BINARY builds fine; only the
# test phase dies, and boot-strap's op=install chains through it. So: let the
# bootstrap run, then if it failed but produced a binary, install that by hand.
# make.py's own version check then passes and it skips bootstrapping entirely.
ensure_bmake() {
    id="$OBJDIR/bmake-install"
    bd="$OBJDIR/bmake-build"
    # make.py skips its bootstrap when the installed bmake's version matches the
    # source's AND .make-py-config holds the exact configure-args string it would
    # have used. That string is fully deterministic, so we write it ourselves
    # rather than hope its own bootstrap survives.
    #
    # DO NOT "fix" --with-machine=amd64 below. Yes, we build aarch64. That value
    # is hardcoded upstream in tools/build/make.py (with its own `# TODO?` beside
    # it) and it configures BMAKE ITSELF — the tool that runs the build — not the
    # target. FreeBSD's build overrides MACHINE from TARGET/TARGET_ARCH anyway;
    # the bootstrap bmake lands in bmake-build/darwin25-arm64/ and the kernel it
    # produces is correctly ARM aarch64. We mirror the string BYTE FOR BYTE
    # because make.py compares it verbatim — "correcting" it to aarch64 makes the
    # comparison fail and make.py re-bootstraps every single run, which is the
    # failure this whole function exists to avoid.
    cfg="--with-default-sys-path=.../share/mk:$id/share/mk --with-machine=amd64 --without-filemon --prefix=$id"

    if [ -x "$id/bin/bmake" ] && [ "$(cat "$id/.make-py-config" 2>/dev/null)" = "$cfg" ]; then
        ok "bootstrap bmake already installed"
        return 0
    fi

    head_ "bootstrapping bmake (its unit tests fail on macOS; that is expected)"
    mkdir -p "$bd"
    # op=build compiles bmake and then runs the test suite. cmd-interrupt and
    # deptgt-interrupt fail because Darwin does not deliver SIGINT the way they
    # expect, so this exits non-zero — but the binary we want is already built.
    ( cd "$bd" && sh "$FBSD_SRC/contrib/bmake/boot-strap" op=build $cfg ) \
        > "$LOGS/bmake.log" 2>&1
    built=$(find "$bd" -name bmake -type f -perm -u+x 2>/dev/null | head -1)
    [ -n "$built" ] || die "bmake did not build at all" \
"See $LOGS/bmake.log. This is a real failure, not the known macOS test issue —
that one still leaves a working bmake binary behind."

    mkdir -p "$id/bin" "$id/share/mk"
    cp "$built" "$id/bin/bmake" && chmod 755 "$id/bin/bmake"
    cp "$FBSD_SRC"/contrib/bmake/mk/* "$id/share/mk/" 2>/dev/null
    printf '%s' "$cfg" > "$id/.make-py-config"

    # Prove make.py now accepts it and does NOT try to bootstrap again — that
    # retry is exactly what the hand-install exists to prevent.
    out=$( cd "$FBSD_SRC" && env MAKEOBJDIRPREFIX="$OBJDIR" python3 tools/build/make.py \
            TARGET=arm64 TARGET_ARCH=aarch64 -V MAKE_VERSION 2>&1 )
    echo "$out" | grep -q 'Bootstrapping bmake' \
        && die "make.py still wants to bootstrap bmake after the hand-install" \
"Its skip check did not accept $id/.make-py-config.
Expected contents:
    $cfg"
    echo "$out" | tail -1 | grep -qE '^[0-9]+$' \
        || die "make.py did not return a MAKE_VERSION" "Got: $(echo "$out" | tail -3)"
    ok "installed bmake by hand around the macOS test failures (v$(echo "$out" | tail -1))"
}

ph_kernel() {
    ensure_bmake
    # kernel-toolchain first: buildkernel needs config(8) and the cross tools,
    # and without it the failure is a confusing "config: command not found".
    head_ "building the kernel toolchain (~6 min)"
    ( cd "$FBSD_SRC" && env MAKEOBJDIRPREFIX="$OBJDIR" python3 tools/build/make.py \
        TARGET=arm64 TARGET_ARCH=aarch64 -j"$JOBS" kernel-toolchain ) \
        > "$LOGS/kernel-toolchain.log" 2>&1 \
        || die "kernel-toolchain failed" "See $LOGS/kernel-toolchain.log"
    ok "kernel toolchain"

    head_ "cross-building the FreeBSD kernel (~2-3 min)"
    ( cd "$FBSD_SRC" && env MAKEOBJDIRPREFIX="$OBJDIR" python3 tools/build/make.py \
        TARGET=arm64 TARGET_ARCH=aarch64 -j"$JOBS" buildkernel KERNCONF=GENERIC ) \
        > "$LOGS/buildkernel.log" 2>&1 \
        || die "buildkernel failed" "See $LOGS/buildkernel.log"

    k=$(find "$OBJDIR" -path '*arm64.aarch64/sys/GENERIC/kernel' -type f | head -1)
    [ -n "$k" ] || die "buildkernel exited 0 but produced no kernel" "See $LOGS/buildkernel.log"
    # Prove it is an aarch64 kernel AND that it is ours, not stock: the Laminar
    # scheduler only exists on the atrium-os branch.
    file "$k" | grep -q 'ARM aarch64' \
        || die "$k is not an aarch64 binary" "Something built for the wrong target."
    nm_bin=$(brew --prefix llvm)/bin/llvm-nm
    n=$("$nm_bin" "$k" 2>/dev/null | grep -ci laminar)
    [ "${n:-0}" -gt 0 ] || die "the kernel has no Laminar symbols" \
"That means stock FreeBSD was built rather than the Atrium fork. Check
freebsd-src is on the atrium-os branch."
    ok "kernel built ($n Laminar symbols) -> $k"
    echo "$k" > "$STATE/kernel.path"
}

ph_kmod() {
    kd="$REPO/atrium-tessera/kmod"
    head_ "cross-building tessera_fs.ko"
    rm -f "$kd/tessera_fs.ko"
    ( cd "$FBSD_SRC" && env MAKEOBJDIRPREFIX="$OBJDIR" python3 tools/build/make.py \
        TARGET=arm64 TARGET_ARCH=aarch64 buildenv \
        BUILDENV_SHELL="/bin/sh -c 'cd $kd && make -j$JOBS SYSDIR=$FBSD_SRC/sys'" ) \
        > "$LOGS/kmod.log" 2>&1 \
        || die "tessera_fs.ko build failed" "See $LOGS/kmod.log"
    [ -f "$kd/tessera_fs.ko" ] || die "kmod build exited 0 but produced no .ko" "See $LOGS/kmod.log"

    # A parallel make can link a .ko before one of its .o files has compiled, and
    # still exit 0 — this has actually happened here. Gate on the symbols instead:
    # the NEON backend and one function from the FS proper must both be present.
    nm_bin=$(brew --prefix llvm)/bin/llvm-nm
    for sym in blake3_hash_many_neon tessera_vop_mknod; do
        "$nm_bin" "$kd/tessera_fs.ko" 2>/dev/null | grep -q "$sym" \
            || die "tessera_fs.ko is missing the symbol $sym" \
"The module linked without all of its objects — a known parallel-make race.
Re-run with --force --only kmod."
    done
    ok "tessera_fs.ko built and symbol-checked"
}

ph_userspace() {
    rustup target add "$TARGET_TRIPLE" >/dev/null 2>&1 || true
    # Each of these is its own cargo workspace with its own target/ dir.
    # (crate-dir, cargo args, produced binaries)
    set -- \
      "portcullis|-p portcullisd|portcullisd" \
      "portcullis|-p portcullis-cli|portcullis" \
      "portcullis|-p opifex|opifex" \
      "frescod|--bin frescod|frescod" \
      "forum-wm||forum-wm" \
      "forum-bar||forum-bar" \
      "forum-dock||forum-dock"
    for spec in "$@"; do
        dir=$(echo "$spec" | cut -d'|' -f1)
        args=$(echo "$spec" | cut -d'|' -f2)
        bins=$(echo "$spec" | cut -d'|' -f3)
        [ -d "$REPO/$dir" ] || { warn "$dir not in this tree, skipping"; continue; }
        printf '  building %-12s' "$(echo "$bins" | cut -d' ' -f1)"
        # shellcheck disable=SC2086
        ( cd "$REPO/$dir" && cargo build --release --target "$TARGET_TRIPLE" $args ) \
            >> "$LOGS/userspace.log" 2>&1 \
            || die "cargo build failed in $dir" "See $LOGS/userspace.log"
        for b in $bins; do
            bp="$REPO/$dir/target/$TARGET_TRIPLE/release/$b"
            [ -x "$bp" ] || die "$dir built but $b is missing" "Expected $bp"
        done
        printf '%sok%s\n' "$GRN" "$R"
    done
    ok "userspace built for $TARGET_TRIPLE"
}

ph_stage() {
    rm -rf "$DIST"; mkdir -p "$DIST/bin" "$DIST/boot" "$DIST/kmod"
    k=$(cat "$STATE/kernel.path" 2>/dev/null)
    [ -n "$k" ] && [ -f "$k" ] && cp "$k" "$DIST/boot/kernel"
    cp "$REPO/atrium-tessera/kmod/tessera_fs.ko" "$DIST/kmod/" 2>/dev/null
    for spec in portcullis:portcullisd portcullis:portcullis portcullis:opifex \
                frescod:frescod forum-wm:forum-wm forum-bar:forum-bar forum-dock:forum-dock; do
        d=${spec%%:*}; b=${spec##*:}
        bp="$REPO/$d/target/$TARGET_TRIPLE/release/$b"
        [ -x "$bp" ] && cp "$bp" "$DIST/bin/$b"
    done

    # Verify every staged file is a FreeBSD aarch64 object, not a macOS one. It
    # is easy to copy a host build by mistake, and the mistake only shows up
    # much later as an exec failure inside the guest.
    bad=''
    for f in "$DIST"/bin/* "$DIST"/kmod/* "$DIST"/boot/*; do
        [ -f "$f" ] || continue
        file "$f" | grep -q 'ARM aarch64' || bad="$bad $(basename "$f")"
    done
    [ -z "$bad" ] || die "staged files are not aarch64:$bad" \
        "A host build was copied in by mistake. Re-run --only userspace --force."
    n=$(find "$DIST" -type f | wc -l | tr -d ' ')
    ok "staged $n files under dist/"
}

# --------------------------------------------------------------------- main ---
ONLY=''; FROM=''; FORCE=''
while [ $# -gt 0 ]; do
    case $1 in
    --list)  cmd_list; exit 0;;
    --only)  shift; ONLY=${1:-};;
    --from)  shift; FROM=${1:-};;
    --force) FORCE=1;;
    -h|--help) usage; exit 0;;
    *) say "unknown option: $1"; usage; exit 2;;
    esac
    shift
done
export FORCE

started=$(date +%s)
say "${B}Atrium bootstrap${R}  repo=$REPO  jobs=$JOBS"

reached=''
for p in $PHASES; do
    [ -n "$ONLY" ] && [ "$ONLY" != "$p" ] && continue
    if [ -n "$FROM" ] && [ -z "$reached" ]; then
        [ "$FROM" = "$p" ] && reached=1 || continue
    fi
    if is_done "$p" && [ -z "$FORCE" ] && [ -z "$ONLY" ]; then
        say "  ${GRN}skip${R} $p (done $(cat "$(done_stamp "$p")"))"
        continue
    fi
    head_ "$p — $(phase_desc "$p")"
    "ph_$p" || exit 1
    mark_done "$p"
done

el=$(( $(date +%s) - started ))
say ""
say "${B}Done${R} in $((el/60))m$((el%60))s."

# A single-phase run is a developer poking at one step; don't bury the result
# under the full getting-started footer.
if [ -n "$ONLY" ]; then
    exit 0
fi

say ""
say "Staged artifacts are in dist/:"
[ -d "$DIST" ] && find "$DIST" -type f | sed "s|$REPO/|  |" | sort
say ""
say "Nothing has been booted. Next:"
say "  sh scripts/run-vm.sh          boot the VM"
say "  sh scripts/vssh               ssh into it once it is up"
say ""
say "Re-run this script any time; finished phases are skipped."
