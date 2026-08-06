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
#   ATRIUM_VM_DIR     where the VM disks and firmware live (default vm/)
#
# Anything already present at the default path is reused too — the override
# exists for when it lives somewhere else entirely.
# ---------------------------------------------------------------------------
TARBALLS="${ATRIUM_TARBALLS:-$REPO/tarballs}"
SYSROOT="${ATRIUM_SYSROOT:-$REPO/sysroot}"
FBSD_SRC="${ATRIUM_FBSD_SRC:-$REPO/freebsd-src/usr/src}"
QEMU_DIR="${ATRIUM_QEMU_DIR:-$REPO/external/qemu-build}"
OBJDIR="${ATRIUM_OBJDIR:-$STATE/fbsdobj}"
VM_DIR="${ATRIUM_VM_DIR:-$REPO/vm}"

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
PHASES="preflight clone fetch sysroot qemu kernel kmod corelib corecheck userspace image stage"

# Phases that RUN EVERY TIME, never skipped by their done-stamp. A test phase
# that runs once and is then skipped forever is worse than no test phase: it
# reports green from a stamp written weeks ago. These still stamp (so --list
# shows when they last ran), they just never short-circuit.
ALWAYS_PHASES="corecheck"

phase_desc() {
    case $1 in
    preflight) echo "check the host has what it needs";;
    clone)     echo "clone freebsd-src (atrium-os) and the patched QEMU";;
    fetch)     echo "download the FreeBSD $FBSD_VER distribution sets";;
    sysroot)   echo "extract base.txz into sysroot/ for cross-compiling";;
    qemu)      echo "build the Atrium-patched qemu-system-aarch64";;
    kernel)    echo "cross-build the FreeBSD kernel on this Mac";;
    kmod)      echo "cross-build the Tessera filesystem module";;
    corelib)   echo "cross-build libtessera_core.a (the Rust tools link against it)";;
    corecheck) echo "run the C core test suite natively (every build)";;
    userspace) echo "cross-build the Rust userspace (frescod, portcullis, forum apps)";;
    image)     echo "create the VM disks and EFI firmware run-vm.sh needs";;
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
    # ★ #114: fail the build if the oversized-stack-frame count grows. An
    # arm64 kernel stack overflow faults in a loop instead of panicking, so a
    # new large stack object on a deep path presents as an unkillable 100%-CPU
    # thread with no console output — very expensive to diagnose, trivial to
    # catch here.
    if ! "$REPO/scripts/check-frames.sh" "$LOGS/kmod.log"; then
        die "kernel stack frames regressed" \
"A new oversized stack frame was introduced. See the list above.
Heap-allocate the object, or mark the callee __noinline, then re-run."
    fi

    ok "tessera_fs.ko built and symbol-checked"
}

# libtessera_core.a — the C core the Rust tessera-tools (fsck, repack, stat,
# reindex, ...) link against. Without it NONE of them link: cargo reports ~100
# undefined symbols and the failure looks like a Rust problem, which is why it
# went unnoticed. A newcomer following the runbook could not build fsck at all.
#
# The Makefile is FreeBSD make syntax, so macOS make dies on it with "missing
# separator" — use the Homebrew bmake we already require, and hand it the cross
# toolchain explicitly so the archive is aarch64-FreeBSD rather than host code.
ph_corelib() {
    cd_core="$REPO/atrium-tessera/core"
    [ -d "$cd_core" ] || { warn "atrium-tessera/core not in this tree, skipping"; return 0; }
    L=$(brew --prefix llvm)
    # ★ Remove the archive first. bmake compares it against the .o files and
    # prints "`libtessera_core.a' is up to date", so --force forced nothing —
    # which is how an EMPTY archive survived repeated rebuilds. (It was
    # emptied by running `bmake check` on macOS, where the host-side test
    # target re-archives into the same in-tree path.)
    rm -f "$cd_core/libtessera_core.a"
    head_ "cross-building libtessera_core.a"
    ( cd "$cd_core" && bmake -s libtessera_core.a \
        CC="$L/bin/clang" AR="$L/bin/llvm-ar" RANLIB="$L/bin/llvm-ranlib" \
        CFLAGS="-O2 -fPIC -fno-strict-aliasing --target=aarch64-unknown-freebsd16.0 \
                --sysroot=$SYSROOT -isystem $SYSROOT/usr/include \
                -I$cd_core/include -I$cd_core/src -D__libtessera_core__" ) \
        > "$LOGS/corelib.log" 2>&1 \
        || die "libtessera_core build failed" "See $LOGS/corelib.log"

    a="$cd_core/libtessera_core.a"
    [ -f "$a" ] || die "bmake exited 0 but produced no libtessera_core.a" "See $LOGS/corelib.log"
    # Prove it is TARGET code. A host-built archive links "fine" right up until
    # the tools are run in the guest.
    "$L/bin/llvm-nm" "$a" >/dev/null 2>&1 \
        || die "$a is not a readable archive" "See $LOGS/corelib.log"
    # ★ An EMPTY archive used to pass this gate. `ar t | head -1` returned "",
    # the extraction silently failed, `[ -f /tmp/ ]` was false, and the whole
    # architecture check was SKIPPED — reported as
    #     ok  libtessera_core.a (0 KiB, aarch64)
    # Missing evidence must never read as success. Count members first, and
    # make the arch check mandatory rather than conditional.
    nmemb=$("$L/bin/llvm-ar" t "$a" 2>/dev/null | grep -c '\.o$')
    [ "${nmemb:-0}" -ge 15 ] || die \
        "libtessera_core.a holds $nmemb object(s) — it is empty or truncated" \
        "Expected one per core/src/*.c. See $LOGS/corelib.log"
    obj=$("$L/bin/llvm-ar" t "$a" 2>/dev/null | head -1)
    "$L/bin/llvm-ar" x "$a" "$obj" --output=/tmp 2>/dev/null
    [ -f "/tmp/$obj" ] || die "could not extract $obj from libtessera_core.a" \
        "The gate cannot verify the architecture, so it must not pass."
    file "/tmp/$obj" | grep -q 'ARM aarch64' \
        || die "libtessera_core.a contains host objects, not aarch64" \
               "The cross flags did not reach bmake. See $LOGS/corelib.log"
    rm -f "/tmp/$obj"
    ok "libtessera_core.a ($(( $(stat -f %z "$a") / 1024 )) KiB, aarch64, $nmemb objects)"
}

# Run the C core's own test suite on the BUILD HOST, every build.
#
# These are cheap (~10 s) and they are the only tests in this tree that can run
# without booting the VM. Keeping them in the bootstrap is what stops them
# rotting: test_quota asserted a default that had been changed two days earlier
# and nobody saw it, because on macOS the suite could not even be linked
# (f015b588). A suite nothing runs is documentation, not verification.
#
# Deliberately AFTER corelib: a cross-build failure should surface before a
# test failure, since the tests exercise a separately compiled host archive and
# cannot tell you anything about the cross artifacts.
ph_corecheck() {
    cd_core="$REPO/atrium-tessera/core"
    [ -d "$cd_core" ] || { warn "atrium-tessera/core not in this tree, skipping"; return 0; }
    command -v cc >/dev/null 2>&1 || { warn "no host cc, skipping core tests"; return 0; }
    head_ "running the C core test suite on this host"
    if sh "$REPO/scripts/core-host-tests.sh" -k > "$LOGS/corecheck.log" 2>&1; then
        ok "$(grep -a "^host tests:" "$LOGS/corecheck.log")"
    else
        tail -30 "$LOGS/corecheck.log"
        die "core test suite FAILED" "See $LOGS/corecheck.log"
    fi
}

ph_userspace() {
    rustup target add "$TARGET_TRIPLE" >/dev/null 2>&1 || true
    # tessera-sys/build.rs links the static core only when this points at it.
    export TESSERA_CORE_LIB="$REPO/atrium-tessera/core"
    # Each of these is its own cargo workspace with its own target/ dir.
    # (crate-dir, cargo args, produced binaries)
    set -- \
      "portcullis|-p portcullisd|portcullisd" \
      "portcullis|-p portcullis-cli|portcullis" \
      "portcullis|-p opifex|opifex" \
      "frescod|--bin frescod|frescod" \
      "forum-wm||forum-wm" \
      "forum-bar||forum-bar" \
      "forum-dock||forum-dock" \
      "atrium-tessera/rs|-p tessera-tools|tessera-fsck tessera-stat tessera-repack tessera-reindex"
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

# Disk set run-vm.sh expects. Sizes match the existing dev VM's volumes. These
# are SPARSE raw files: creating a 25G volume costs kilobytes until it is used.
#   name                       bytes         what it is
AUX_DISKS="crash-test.img:268435456 tessera-storage.img:17179869184 \
tessera-root.img:3221225472 tessera-devroot.img:26843545600"
VM_DISK_GROW="60G"

ph_image() {
    vmd="$VM_DIR"
    mkdir -p "$vmd"

    # ---- the root disk -----------------------------------------------------
    # NEVER overwrite an existing vm.qcow2. On a dev machine that file is the
    # working VM with real state in it (~29G here), and --force is meant for
    # redoing BUILDS, not for destroying disks. Replacing it has to be a
    # separate, deliberate act.
    # -s not -e: a zero-byte vm.qcow2 is a failed earlier attempt, not a VM
    # worth protecting. A real one always has content.
    if [ -s "$vmd/vm.qcow2" ]; then
        ok "vm.qcow2 already exists — left untouched"
        say "     (to rebuild it from a fresh FreeBSD image, move it aside first;"
        say "      --force deliberately does NOT overwrite disks)"
    else
        img="FreeBSD-${FBSD_VER}-arm64-aarch64-ufs.qcow2.xz"
        url="${ATRIUM_VM_IMAGE_URL:-https://download.freebsd.org/snapshots/VM-IMAGES/${FBSD_VER}/aarch64/Latest/$img}"
        if [ ! -s "$TARBALLS/$img" ]; then
            head_ "downloading the FreeBSD VM image (~500 MB)"
            curl -fL --progress-bar -o "$TARBALLS/$img.part" "$url" \
                || die "downloading $img failed" \
"Tried: $url
Snapshot builds rotate, so this can 404. Browse
  https://download.freebsd.org/snapshots/VM-IMAGES/${FBSD_VER}/aarch64/Latest/
and set ATRIUM_VM_IMAGE_URL to the -ufs.qcow2.xz that is actually there."
            mv "$TARBALLS/$img.part" "$TARBALLS/$img"
        else
            ok "reusing $img"
        fi
        head_ "decompressing into vm/vm.qcow2"
        xz -dc "$TARBALLS/$img" > "$vmd/vm.qcow2.part" \
            || die "decompressing $img failed" "The download may be truncated; delete it and re-run."
        mv "$vmd/vm.qcow2.part" "$vmd/vm.qcow2"
        # The stock image is sized to its contents; give the guest room to build in.
        qemu="${ATRIUM_QEMU_BIN:-$QEMU_DIR/build/qemu-system-aarch64}"
        qimg="$(dirname "$qemu")/qemu-img"
        [ -x "$qimg" ] || qimg=$(command -v qemu-img)
        [ -n "$qimg" ] && "$qimg" resize "$vmd/vm.qcow2" "$VM_DISK_GROW" >/dev/null 2>&1 \
            && ok "root disk grown to $VM_DISK_GROW" \
            || warn "could not resize vm.qcow2 (no qemu-img); the guest will have less room"
        ok "vm/vm.qcow2 created from stock FreeBSD $FBSD_VER"
    fi

    # ---- auxiliary volumes -------------------------------------------------
    # Scratch/Tessera volumes. The guest formats these itself; here they only
    # have to exist at the right size, or qemu refuses to start.
    for spec in $AUX_DISKS; do
        n=${spec%%:*}; sz=${spec##*:}
        if [ -e "$vmd/$n" ]; then ok "$n exists"; continue; fi
        # Sparse: seek to size-1 and write one byte.
        dd if=/dev/zero of="$vmd/$n" bs=1 count=1 seek=$((sz - 1)) >/dev/null 2>&1 \
            || die "could not create $vmd/$n" "Check free space and permissions."
        ok "created $n ($((sz / 1073741824))G sparse)"
    done

    # ---- EFI firmware ------------------------------------------------------
    # qemu ships a 2 MiB edk2 image; the -drive if=pflash unit wants exactly
    # 64 MiB, so it is zero-padded. run-vm.sh points at a path inside
    # build/qemu-bundle/ that is a symlink into build/pc-bios/ and is BROKEN in a
    # fresh build tree — the real file is in the source tree's pc-bios/. Try both.
    if [ -s "$vmd/edk2-aarch64-code.fd" ]; then
        ok "EFI code firmware present"
    else
        src=''
        for c in "$QEMU_DIR/build/qemu-bundle/opt/homebrew/share/qemu/edk2-aarch64-code.fd" \
                 "$QEMU_DIR/build/pc-bios/edk2-aarch64-code.fd" \
                 "$QEMU_DIR/pc-bios/edk2-aarch64-code.fd"; do
            [ -s "$c" ] && { src=$c; break; }
        done
        [ -n "$src" ] || die "cannot find edk2-aarch64-code.fd" \
"Looked under $QEMU_DIR. The qemu phase must run before this one."
        dd if=/dev/zero of="$vmd/edk2-aarch64-code.fd" bs=1m count=64 >/dev/null 2>&1
        dd if="$src" of="$vmd/edk2-aarch64-code.fd" conv=notrunc >/dev/null 2>&1 \
            || die "padding the EFI firmware failed" "Source was $src"
        ok "EFI code firmware padded to 64 MiB from $(basename "$(dirname "$src")")/"
    fi
    if [ -s "$vmd/edk2-arm-vars.fd" ]; then
        ok "EFI vars present"
    else
        # This is UEFI's NVRAM (boot entries, boot order), so it must look like a
        # BLANK FLASH CHIP, and erased NOR flash reads as 0xFF — not 0x00. A
        # zero-filled store is not an erased one; EDK2 has to decide the header is
        # invalid and reformat, and the failure mode if it does not is a VM that
        # simply will not boot. The repo's own edk2-vars-blank.fd is 0xFF-filled,
        # so prefer that template and only synthesise one as a fallback.
        if [ -s "$vmd/edk2-vars-blank.fd" ]; then
            cp "$vmd/edk2-vars-blank.fd" "$vmd/edk2-arm-vars.fd" \
                || die "could not copy the blank vars template" "Check permissions."
            ok "EFI vars store created from edk2-vars-blank.fd"
        else
            # 64 MiB of 0xFF without depending on GNU tools.
            perl -e 'print "\xff" x (1024*1024) for 1..64' > "$vmd/edk2-arm-vars.fd" \
                || die "could not create the EFI vars store" "Check free space."
            ok "EFI vars store created (64 MiB of 0xFF = erased flash)"
        fi
        first=$(xxd -l4 -p "$vmd/edk2-arm-vars.fd" 2>/dev/null)
        [ "$first" = "ffffffff" ] || warn "vars store does not begin 0xFFFFFFFF (got $first)"
    fi

    # ---- prove run-vm.sh has everything it needs ---------------------------
    missing=''
    for f in vm.qcow2 crash-test.img tessera-storage.img tessera-root.img \
             tessera-devroot.img edk2-aarch64-code.fd edk2-arm-vars.fd; do
        [ -s "$vmd/$f" ] || missing="$missing $f"
    done
    [ -z "$missing" ] || die "the VM is still missing:$missing" \
        "run-vm.sh will refuse to start without every one of these."
    for f in edk2-aarch64-code.fd edk2-arm-vars.fd; do
        sz=$(stat -f %z "$vmd/$f")
        [ "$sz" = "67108864" ] || die "$f is $sz bytes, must be exactly 67108864" \
            "qemu's pflash unit requires a 64 MiB image. Delete it and re-run this phase."
    done
    ok "run-vm.sh has every disk and firmware file it needs"
}

ph_stage() {
    rm -rf "$DIST"; mkdir -p "$DIST/bin" "$DIST/boot" "$DIST/kmod"
    k=$(cat "$STATE/kernel.path" 2>/dev/null)
    [ -n "$k" ] && [ -f "$k" ] && cp "$k" "$DIST/boot/kernel"
    cp "$REPO/atrium-tessera/kmod/tessera_fs.ko" "$DIST/kmod/" 2>/dev/null
    for spec in portcullis:portcullisd portcullis:portcullis portcullis:opifex \
                frescod:frescod forum-wm:forum-wm forum-bar:forum-bar forum-dock:forum-dock \
                atrium-tessera/rs:tessera-fsck atrium-tessera/rs:tessera-stat \
                atrium-tessera/rs:tessera-repack atrium-tessera/rs:tessera-reindex; do
        d=${spec%%:*}; b=${spec##*:}
        bp="$REPO/$d/target/$TARGET_TRIPLE/release/$b"
        [ -x "$bp" ] && cp "$bp" "$DIST/bin/$b"
    done

    # ★ The SPIR-V bundles. frescod loads bundles/atrium-core/{compute,pipelines}
    # /*.spv at startup and will not start without them, so staging only the
    # binaries produces an install that looks complete and cannot run a
    # compositor. atrium-text is a sibling frescod picks up on its own.
    # These are ARCH-INDEPENDENT (SPIR-V + JSON), which is why they are staged
    # here, ABOVE the aarch64 check — that check must not see them.
    for b in atrium-core atrium-text; do
        [ -d "$REPO/bundles/$b" ] || continue
        nspv=$(find "$REPO/bundles/$b" -name '*.spv' | wc -l | tr -d ' ')
        if [ "$nspv" -eq 0 ]; then
            warn "bundles/$b has no .spv — build it with bundles/$b/build.sh" \
                 "frescod will fail to start without the compiled shaders."
            continue
        fi
        mkdir -p "$DIST/bundles"
        cp -R "$REPO/bundles/$b" "$DIST/bundles/$b"
        ok "staged bundle $b ($nspv .spv)"
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
    # The staged artifacts still have to get INTO the guest. macOS cannot mount
    # UFS, so this cannot happen from the host — it happens on first boot, over
    # the 9p share, driven by this script. Generated here so it always matches
    # what was actually staged.
    cat > "$DIST/install-atrium.sh" <<'INSTALLER'
#!/bin/sh
# install-atrium.sh — run this INSIDE the VM, once, after the first boot.
#
#   kldload p9fs; mount -t p9fs -o trans=virtio bsd_share /mnt/host
#   sh /mnt/host/dist/install-atrium.sh
#
# Installs the cross-built kernel, the Tessera module and the Atrium userspace
# that scripts/bootstrap-atrium.sh produced on the host.
set -eu
D=$(cd "$(dirname "$0")" && pwd)
[ -d /mnt/host ] || { echo "mount the 9p share first (see the header)"; exit 1; }

echo "== kernel =="
if [ -f "$D/boot/kernel" ]; then
    mkdir -p /boot/atrium
    cp "$D/boot/kernel" /boot/atrium/kernel
    echo "  installed /boot/atrium/kernel (boot it with: boot /boot/atrium/kernel)"
fi

echo "== tessera module =="
if [ -f "$D/kmod/tessera_fs.ko" ]; then
    cp "$D/kmod/tessera_fs.ko" /boot/modules/ 2>/dev/null || cp "$D/kmod/tessera_fs.ko" /boot/kernel/
    # nullfs must be preloaded, not demand-loaded: an on-demand load races jail
    # mounts and panics getnewvnode intermittently.
    grep -q '^nullfs_load' /boot/loader.conf 2>/dev/null || echo 'nullfs_load="YES"' >> /boot/loader.conf
    echo "  installed tessera_fs.ko + ensured nullfs_load=YES"
fi

echo "== userspace =="
mkdir -p /usr/local/bin
for b in "$D"/bin/*; do
    [ -f "$b" ] || continue
    install -m 755 "$b" /usr/local/bin/ && echo "  $(basename "$b")"
done

echo "== bundles =="
# frescod loads bundles/atrium-core/{compute,pipelines}/*.spv at startup and
# refuses to run without them. It searches /usr/local/share/atrium/bundles, so
# that is where they go; atrium-text is a sibling it picks up on its own.
# Prefer the staged copy under dist/, fall back to the repo over the 9p share.
SRCB=""
[ -d "$D/bundles" ] && SRCB="$D/bundles"
[ -z "$SRCB" ] && [ -d "$D/../bundles" ] && SRCB="$D/../bundles"
if [ -n "$SRCB" ]; then
    mkdir -p /usr/local/share/atrium/bundles
    for b in atrium-core atrium-text; do
        [ -d "$SRCB/$b" ] || continue
        rm -rf "/usr/local/share/atrium/bundles/$b"
        cp -R "$SRCB/$b" /usr/local/share/atrium/bundles/
        # Copying dist/ off macOS with tar leaves AppleDouble "._name" siblings.
        # They are junk, and "._op_path.comp.spv" MATCHES *.spv — so they both
        # pollute the bundle and inflate every shader count that looks for it.
        find "/usr/local/share/atrium/bundles/$b" -name '._*' -delete 2>/dev/null
        echo "  $b ($(find /usr/local/share/atrium/bundles/$b -name '*.spv' | wc -l | tr -d ' ') .spv)"
    done
else
    echo "  WARNING no bundles found — frescod will not start."
    echo "  Build them on the host with bundles/atrium-core/build.sh, re-run"
    echo "  bootstrap-atrium.sh --only stage, then re-run this installer."
fi

echo "== verify =="
for b in portcullisd frescod opifex; do
    if [ -x /usr/local/bin/$b ]; then
        ldd /usr/local/bin/$b >/dev/null 2>&1 \
            && echo "  $b: links cleanly" \
            || echo "  $b: WARNING unresolved libraries (ldd failed)"
    fi
done
# Gate on the SHADERS landing, not on the copy having been attempted: an empty
# or partial bundle directory makes frescod fail at startup with a message that
# points at the compositor, not at the install.
core=/usr/local/share/atrium/bundles/atrium-core
n=$(find $core -name '*.spv' 2>/dev/null | wc -l | tr -d ' ')
if [ -f "$core/manifest.json" ] && [ "$n" -gt 0 ]; then
    echo "  atrium-core bundle: $n shaders + manifest.json installed"
else
    echo "  atrium-core bundle: MISSING or incomplete — frescod cannot start"
fi
echo
echo "Done. Atrium userspace is in /usr/local/bin,"
echo "bundles in /usr/local/share/atrium/bundles."
INSTALLER
    chmod 755 "$DIST/install-atrium.sh"

    n=$(find "$DIST" -type f | wc -l | tr -d ' ')
    ok "staged $n files under dist/ (including install-atrium.sh)"
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
    always=''
    case " $ALWAYS_PHASES " in *" $p "*) always=1 ;; esac
    if is_done "$p" && [ -z "$FORCE" ] && [ -z "$ONLY" ] && [ -z "$always" ]; then
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
say "Then, inside the guest, install what was just built:"
say "  kldload p9fs && mount -t p9fs -o trans=virtio bsd_share /mnt/host"
say "  sh /mnt/host/dist/install-atrium.sh"
say ""
say "(macOS cannot mount the guest's UFS, so the artifacts go in over 9p on"
say " first boot rather than being injected into the image from here.)"
say ""
say "Re-run this script any time; finished phases are skipped."
