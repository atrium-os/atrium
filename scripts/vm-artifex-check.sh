#!/bin/sh
# Artifex v0 on screen (dev-tools-plan.md M11) in the amd64 cloud VM with
# frescod headless: Artifex (org.atrium.artifex, `workspace-exec`) shows the
# workspace chooser; a click's press allows `proj`; its work appears as a
# tree; a click opens hello.c (C colours); typing edits it and Ctrl+S saves
# it into the workspace; Ctrl+B builds it in the workspace with cc and the
# compiler's error lands in Problems and the gutter; a click on the problem
# jumps to its line; the Terminal button opens proj's terminal; with clangd
# on the workspace's PATH (the c-dev layer) a diagnostic comes from it too.
# Runs on the HOST after cloud-vm.sh up (QEMU fork, TCG_CPU=Haswell-noTSX).
# CDEV=<c-dev layer tree> installs it first (build-c-dev-layer.sh's output);
# RUST=<Rust layer tree> installs that too, makes proj a Rust workspace and
# checks rust-analyzer on rs/main.rs. Its bin/cargo-atrium must be main's
# (cross-build sdk/cargo-atrium; FRESH_LAYERS=1 reinstalls the layer).
# FUZZ_VENDOR=<dir with vendor/> (default ~/work/fuzz-vendor: libfuzzer-sys
# and what it needs, from `cargo vendor`) adds a photos project: fuzzing its
# core finds a crash, and Move it moves the parser into a worker.
# BUNDLE=1 installs Artifex and the Terminal the platform-SDK way instead:
# `cargo atrium bundle --sign` on the host makes each .insula (programs,
# library closure, loader), and the VM unarchives and installs it through
# opifex's trust gate (needs cargo-atrium and opifex built for the host:
# sdk/target/release and portcullis/target/release).
# Ends "artifex: N failed".
#
#   sh scripts/vm-artifex-check.sh [out-dir]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/artifex-shots}
mkdir -p "$OUT"
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }

echo "=== cross-build"
S=$SYSROOT
export CC_x86_64_unknown_freebsd=clang CXX_x86_64_unknown_freebsd=clang++
export CFLAGS_x86_64_unknown_freebsd="--target=x86_64-unknown-freebsd --sysroot=$S"
export CXXFLAGS_x86_64_unknown_freebsd="--target=x86_64-unknown-freebsd --sysroot=$S"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "artifex: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex -p jaild -p atrium-volumes --bin portcullisd --bin portcullis --bin opifex --bin atrium-jaild --bin atrium-volumes"
xb atrium-term-socket "--bin atrium-term-socket"
xb stoa "-p stoa --bin stoad"
xb artifex "-p artifex --bin artifex"
[ -z "${FORUM:-}" ] || xb forum-wm "--bin forum-wm"

echo "=== install"
B=/var/tmp/artifexchk
V "pkill -x portcullisd; pkill -x frescod; pkill -x forum-wm; pkill -x stoad; pkill -f atrium-term-socket; pkill -x artifex; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
V "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    V "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || scp -q $K -P 2222 "$f" root@localhost:/usr/local/share/fonts/ibm-plex/
done
V "id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/atrium-jaild portcullis/target/$TARGET/release/atrium-volumes \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket \
         stoa/target/$TARGET/release/stoad artifex/target/$TARGET/release/artifex; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "artifex: 1 failed"; exit 1; }
done
scp -q $K -P 2222 "$ROOT/artifex/app/atrium.toml" root@localhost:$B/artifex.toml
scp -q $K -P 2222 "$ROOT/atrium-term-socket/atrium.toml" root@localhost:$B/terminal.toml
if [ -n "${BUNDLE:-}" ]; then
    echo "=== cargo atrium bundle"
    BK=$ROOT/scratch/artifexchk-bundle; mkdir -p "$BK"
    [ -f "$BK/key8.pem" ] || { openssl ecparam -name prime256v1 -genkey -noout -out "$BK/key.pem" && openssl pkcs8 -topk8 -nocrypt -in "$BK/key.pem" -out "$BK/key8.pem"; }
    openssl ec -in "$BK/key8.pem" -pubout -out "$BK/pub.pem" 2>/dev/null
    # The sysroot pulled from the guest has no loader; the bundle carries it.
    [ -f "$S/libexec/ld-elf.so.1" ] || { mkdir -p "$S/libexec" && scp -q $K -P 2222 root@localhost:/libexec/ld-elf.so.1 "$S/libexec/"; }
    for d in artifex/app atrium-term-socket; do
        (cd "$ROOT/$d" && PATH="$ROOT/sdk/target/release:$PATH" OPIFEX="$ROOT/portcullis/target/release/opifex" RUSTFLAGS= \
            cargo atrium bundle --arch amd64 --sysroot "$S" --sign "$BK/key8.pem" --out "$BK/$(basename $d)" >"$BK/$(basename $d).log" 2>&1) \
            || { echo "bundle of $d failed: $(tail -2 "$BK/$(basename $d).log")"; echo "artifex: 1 failed"; exit 1; }
    done
    scp -q $K -P 2222 "$BK/app.insula" root@localhost:$B/artifex.insula && scp -q $K -P 2222 "$BK/atrium-term-socket.insula" root@localhost:$B/terminal.insula \
        && scp -q $K -P 2222 "$BK/pub.pem" root@localhost:$B/sdk-gate.pem || { echo "copy of the bundles failed"; echo "artifex: 1 failed"; exit 1; }
fi
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
TERM_ID=org.atrium.terminal; ART=org.atrium.artifex
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
# portcullisd speaks to jaild and atrium-volumes: main's go with main's portcullisd.
for d in atrium-jaild atrium-volumes; do
  cmp -s $B/\$d /usr/local/bin/\$d || { install -m 0755 $B/\$d /usr/local/bin/\$d; service \$d restart >/dev/null 2>&1 || true; }
done
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
sign() { openssl dgst -sha256 -sign /root/of-key.pem -out \$1/atrium.toml.sig \$1/atrium.toml; }
for id in $TERM_ID $ART; do opifex uninstall \$id >/dev/null 2>&1 || true; rm -rf /home/alice/.atrium/apps/\$id; done
if [ -n "${BUNDLE:-}" ]; then
  # The SDK's archives, as a publisher would ship them. The index covers every
  # file, so the check's two files (trace on, the window at 40,30) go into
  # the installed folder afterwards, as they would on a developer's machine.
  cp $B/sdk-gate.pem /etc/atrium/publishers/sdk-gate.pem
  for x in terminal:$TERM_ID artifex:$ART; do
    n=\${x%%:*}; id=\${x#*:}; rm -rf $B/u-\$n; opifex unarchive $B/\$n.insula $B/u-\$n >/dev/null
    opifex install $B/u-\$n > $B/install-\$n.log 2>&1
    grep -q "installed \$id" $B/install-\$n.log
    echo on > /var/lib/atrium/apps/\$id/.trace
  done
  echo 40,30 > /var/lib/atrium/apps/$ART/.at
else
# Both with their checked-in manifests (the Terminal with trace on; Artifex
# with trace on and the window at (40, 30)).
O=$B/b-$TERM_ID; mkdir -p \$O/bin; cp $B/atrium-term-socket \$O/bin/; cp $B/terminal.toml \$O/atrium.toml
echo on > \$O/.trace
sign \$O; opifex install \$O >/dev/null
O=$B/b-$ART; mkdir -p \$O/bin; cp $B/artifex \$O/bin/; cp $B/artifex.toml \$O/atrium.toml
echo on > \$O/.trace; echo 40,30 > \$O/.at
sign \$O; opifex install \$O >/dev/null
fi
mkdir -p /atrium/sockets/portcullis /atrium/sockets/clipboard /var/run/atrium; rm -f $PSOCK /var/run/atrium/stoad.sock
: > $LOG
daemon -f -o $B/stoad.log env STOA_CTL=/var/run/atrium/stoad.sock STOA_STATE=off $B/stoad
sleep 1; chmod 600 /var/run/atrium/stoad.sock
daemon -f -o $LOG env PORTCULLISD_FRESCO_ACTIVATION=$B/activation.sock /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd, Terminal and Artifex installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "artifex: $fails failed"; exit 1; }
for a in $TERM_ID $ART; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "grep -q AVX2 /var/run/dmesg.boot" || { bad "the VM's CPU has no AVX2: cloud-vm.sh down, then up with QEMU_BIN=\$HOME/qemu-atrium/build/qemu-system-x86_64 TCG_CPU=Haswell-noTSX"; echo "artifex: $fails failed"; exit 1; }
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "artifex: $fails failed"; exit 1; }
inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $B/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-4}; }
ctrl() { inj "KEY 0xe0 1 1" "KEY $1 1 1" "KEY $1 0 1" "KEY 0xe0 0 0"; sleep ${GAP:-4}; }
shot() { sleep 3; get $B/screen.png "$OUT/$1.png" 2>/dev/null || get $B/screen "$OUT/$1.png" 2>/dev/null; [ -s "$OUT/$1.png" ] && echo "  screenshot $1.png" || echo "  (no screenshot for $1)"; }
typ() {
    python3 -I - "$1" > /tmp/artifex-keys.txt <<'PY'
import sys
lo = {c: 0x04 + i for i, c in enumerate("abcdefghijklmnopqrstuvwxyz")}
lo.update({c: 0x1e + i for i, c in enumerate("1234567890")})
lo.update({" ": 0x2c, "-": 0x2d, "=": 0x2e, "/": 0x38, ".": 0x37, ";": 0x33, "'": 0x34, ",": 0x36, "\n": 0x28})
hi = {">": 0x37, "<": 0x36, "_": 0x2d, "|": 0x31, ":": 0x33, '"': 0x34, "&": 0x24, "(": 0x26, ")": 0x27, "*": 0x25}
hi.update({c.upper(): 0x04 + i for i, c in enumerate("abcdefghijklmnopqrstuvwxyz")})
for c in sys.argv[1]:
    if c in lo:
        print(f"KEY {lo[c]:#x} 1"); print(f"KEY {lo[c]:#x} 0")
    elif c in hi:
        print("KEY 0xe1 1 2"); print(f"KEY {hi[c]:#x} 1 2"); print(f"KEY {hi[c]:#x} 0 2"); print("KEY 0xe1 0 0")
PY
    scp -q $K -P 2222 /tmp/artifex-keys.txt root@localhost:$B/keys.txt
    V "nc -N -U $B/inject.sock < $B/keys.txt" >/dev/null
    sleep ${GAP:-4}
}
# Artifex's last state line (ARTIFEX_TRACE format, main.rs).
state() { V "grep 'artifex: choose=' $B/artifex.log | tail -1"; }
waitstate() { # pattern [secs]
    i=0; while [ $i -lt ${2:-60} ] && ! state | grep -q -- "$1"; do sleep 2; i=$((i+2)); done
    state | grep -q -- "$1"
}

# The c-dev layer (clangd) when CDEV names its tree, and the Rust layer
# (rust-analyzer) when RUST does: signed with a gate key standing in for
# the platform key, as vm-layers-check.sh does. A workspace made after
# them has them on PATH (proj is a Rust workspace when RUST is set: Rust
# gets c-dev too).
CDEV=${CDEV:-}
RUST=${RUST:-}
[ -n "$CDEV" ] && [ -f "$CDEV/atrium.toml" ] || CDEV=
[ -n "$RUST" ] && [ -f "$RUST/atrium.toml" ] || RUST=
RVER=$( [ -z "$RUST" ] || sed -n 's/^version = "\(.*\)"/\1/p' "$RUST/atrium.toml" | head -1)
# Installed already (from an earlier run): kept, unless FRESH_LAYERS=1.
if [ -z "${FRESH_LAYERS:-}" ]; then
    L=$(V "opifex layer list")
    [ -z "$CDEV" ] || ! echo "$L" | grep -q " c-dev@1 " || { ok "c-dev already installed"; CDEV=; KEEP_CDEV=1; }
    [ -z "$RUST" ] || ! echo "$L" | grep -q " rust@$RVER " || { ok "rust already installed"; RUST=; KEEP_RUST=1; }
fi
if [ -n "$CDEV$RUST" ]; then
    echo "=== the layers"
    (cd "$ROOT/portcullis" && cargo build -q -p opifex) || { echo "host opifex failed"; exit 1; }
    LH=$(mktemp -d)
    openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out "$LH/gate.key"
    openssl ec -in "$LH/gate.key" -pubout -out "$LH/gate.pem" 2>/dev/null
    scp -q $K -P 2222 "$LH/gate.pem" root@localhost:$B/
    # Nothing may hold the old layers: stop the workspaces, drop their mounts.
    V "mkdir -p /etc/atrium/platform-keys && cp $B/gate.pem /etc/atrium/platform-keys/artifex-gate.pem && \
       for w in scratch proj; do su -m alice -c \"portcullis workspace stop \$w\" >/dev/null 2>&1; done; \
       for j in \$(jls name | grep '^ws_alice_'); do jail -r \$j; done; \
       for m in \$(mount -p | awk '\$2 ~ /toolchains/ {print \$2}' | sort -r); do umount -f \$m; done" >/dev/null 2>&1
    layer() { # tree name@version check
        n=${2%@*}
        # The manifest without its content line (the [layer.env] after it stays).
        grep -v '^content = ' "$1/atrium.toml" > "$LH/$n.toml"
        "$ROOT/portcullis/target/debug/opifex" layer pack "$1" --manifest "$LH/$n.toml" --sign "$LH/gate.key" >/dev/null || { bad "pack $n"; return; }
        (cd "$(dirname "$1")" && tar -czf "$LH/$n.tar.gz" "$(basename "$1")")
        want=$(sha256sum "$LH/$n.tar.gz" | cut -d' ' -f1)
        scp -q $K -P 2222 "$LH/$n.tar.gz" root@localhost:$B/
        rm -f "$LH/$n.tar.gz"
        [ "$want" = "$(V "sha256 -q $B/$n.tar.gz")" ] || { bad "$n arrived damaged (TCG scp); run again"; return; }
        out=$(V "cd $B && rm -rf $(basename "$1") && tar -xf $n.tar.gz && rm $n.tar.gz && \
           (opifex layer remove $2 >/dev/null 2>&1; opifex layer install $B/$(basename "$1")) && rm -rf $B/$(basename "$1")" 2>&1)
        V "opifex layer list" | grep -q " $2 " && V "grep -q $3 /var/lib/atrium/layers/$2/atrium.toml" \
            && ok "$n installed" || bad "$n: $out"
    }
    [ -z "$CDEV" ] || layer "$CDEV" c-dev@1 LD_LIBRARY_PATH
    [ -z "$RUST" ] || layer "$RUST" "rust@$RVER" RUST_SRC_PATH
    rm -rf "$LH"
fi
[ -z "${KEEP_RUST:-}" ] || RUST=kept

echo "=== the workspace"
# The fuzz harness's crates (libfuzzer-sys and what it needs), vendored on
# the host: the workspace has no network. FUZZ_VENDOR=<dir with vendor/>.
FUZZ_VENDOR=${FUZZ_VENDOR:-$HOME/work/fuzz-vendor}
if [ -d "$FUZZ_VENDOR/vendor" ]; then
    (cd "$FUZZ_VENDOR" && tar -czf /tmp/artifex-vendor.tgz vendor) && scp -q $K -P 2222 /tmp/artifex-vendor.tgz root@localhost:$B/vendor.tgz
fi
V "rm -rf $B/c-app; mkdir -p $B/c-app" && scp -q $K -P 2222 "$ROOT"/sdk/templates/c-app/*.tmpl root@localhost:$B/c-app/
V "service atrium-jaild status >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1; sleep 1; test -S /var/run/atrium/jaild.sock" && ok "jaild is up" || bad "no jaild"
W=/home/alice/.atrium/workspaces
for w in scratch proj; do V "su -m alice -c 'portcullis workspace stop $w'" >/dev/null 2>&1; done
# A dev app an earlier run started (Run refuses while it runs).
V "for j in \$(jls name | grep '^dev_alice_'); do jail -r \$j; done" >/dev/null 2>&1
V "for m in \$(mount -p | awk '\$2 ~ /workspaces\/alice|jails\/$ART\/work/ {print \$2}' | sort -r); do umount -f \$m; done; rm -rf $W /var/lib/atrium/workspaces/alice /var/db/atrium/alice/workspace-picks /var/run/atrium/ide"
V "su -m alice -c 'portcullis workspace list'" >/dev/null 2>&1
V "su -m alice -c 'portcullis workspace new proj --network none${RUST:+ --toolchain rust}'" >/dev/null 2>&1
V 'sh -s' <<EOS
set -e
cat > $W/proj/work/hello.c <<'C'
#include <stdio.h>

int main(void) {
	printf("hello\n");
	return missing;
}
C
printf '# proj\n' > $W/proj/work/README.md
# A Cargo project for rust-analyzer: a module it can't find is its own
# diagnostic, and the type error is cargo check's.
mkdir -p $W/proj/work/rs
printf '[package]\nname = "rs"\nversion = "0.1.0"\nedition = "2021"\n\n[[bin]]\nname = "rs"\npath = "main.rs"\n' > $W/proj/work/rs/Cargo.toml
printf 'mod missing;\n\nfn main() {\n    let n: i32 = "one";\n    println!("{n}");\n}\n' > $W/proj/work/rs/main.rs
# A C app from the c-app template (cargo atrium new --template c-app), for
# the composition panel and Run.
if [ -f /usr/share/mk/atrium.app.mk ]; then
    mkdir -p $W/proj/work/app
    T=$B/c-app
    sed 's/{{name}}/app/g' \$T/Makefile.tmpl > $W/proj/work/app/Makefile
    sed 's/{{name}}/app/g; s/{{title}}/App/g; s/{{id}}/org.example.app/g' \$T/atrium.toml.tmpl > $W/proj/work/app/atrium.toml
    sed 's/{{name}}/app/g; s/{{title}}/App/g' \$T/main.c.tmpl > $W/proj/work/app/app.c
fi
# A Rust app whose core reads album bytes itself (a parser in the core):
# the fuzz panel's core crash, and Move into a worker.
if [ -f $B/vendor.tgz ]; then
    P=$W/proj/work/photos
    mkdir -p \$P/src \$P/.cargo
    tar -xzf $B/vendor.tgz -C \$P
    printf '[source.crates-io]\nreplace-with = "vendored"\n\n[source.vendored]\ndirectory = "vendor"\n' > \$P/.cargo/config.toml
    printf '[package]\nname = "photos"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n\n[package.metadata.atrium.fuzz."core:album"]\nfunction = "photos::read_album"\nabout = "album bytes"\n' > \$P/Cargo.toml
    printf '[app]\nid = "org.example.photos"\nentry = "bin/photos"\n' > \$P/atrium.toml
    printf 'pub fn read_album(b: &[u8]) {\n    if b.len() > 3 && b[0] == 0x41 {\n        let n = b[1] as usize;\n        let _ = b[n];\n    }\n}\n' > \$P/src/lib.rs
fi
chown -R alice:files-alice $W/proj/work; chmod -R g+rwX $W/proj/work
EOS
check "proj made with hello.c" "1" "$(V "test -f $W/proj/work/hello.c && echo 1")"

echo "=== Artifex"
V ": > $B/artifex.log; daemon -f -o $B/artifex.log su -m alice -c 'portcullis launch --no-prompt $ART'"
waitstate "choose=true" 90 && ok "it opens on the workspace chooser ($(state | sed 's/.*choose=true \[\([^]]*\)\].*/\1/'))" || bad "chooser: $(state) $(V "tail -3 $B/artifex.log")"
shot artifex-0-chooser
# The row of proj in the chooser (ui.rs: centred 420 wide, rows 48 apart).
ROW=$(state | sed 's/.*choose=true \[\([^]]*\)\].*/\1/' | tr ',' '\n' | grep -n '^proj$' | cut -d: -f1)
ROW=$(( ${ROW:-2} - 1 ))
press 735 $((351 + 48 * ROW))
waitstate 'ws=Some("proj")' 60 && ok "a click's press allows proj" || bad "pick: $(state) $(V "grep -i 'pick\|REFUSED' $LOG | tail -2")"
check "remembered for Artifex" "$ART	proj" "$(V "cat /var/db/atrium/alice/workspace-picks")"
waitstate 'rows=\[[^]]*hello.c' 20 && ok "its work is the tree" || bad "tree: $(state)"
shot artifex-1-tree

echo "=== edit and save"
# hello.c's row (the workspace may hold .atrium and a README too).
ROW=$(state | sed 's/.*rows=\[\([^]]*\)\].*/\1/' | tr ',' '\n' | grep -n '^hello.c$' | cut -d: -f1)
press 100 $((89 + 22 * (${ROW:-1} - 1)))
waitstate 'tabs=\[hello.c\]' 20 && ok "a click opens hello.c" || bad "open: $(state)"
shot artifex-2-open
typ "// edited in artifex
"
waitstate 'tabs=\[hello.c\*\]' 20 && ok "typing marks it changed" || bad "dirty: $(state)"
ctrl 0x16                  # Ctrl+S
waitstate 'tabs=\[hello.c\] .*Saved hello.c' 20 && ok "Ctrl+S saves it" || bad "save: $(state)"
check "the workspace has the edit" "// edited in artifex" "$(V "head -1 $W/proj/work/hello.c")"
check "still the person's file" "alice" "$(V "stat -f %Su $W/proj/work/hello.c" 2>/dev/null)"

echo "=== build"
ctrl 0x05                  # Ctrl+B
waitstate 'running=None problems=[1-9]' 120 && ok "Ctrl+B builds in proj and the error is a problem" || bad "build: $(state)"
waitstate 'diags=[1-9]' 10 && ok "and marks the editor" || bad "diags: $(state)"
shot artifex-3-problems
GAP=6 press 440 508        # the first problem (panel under the editor)
waitstate 'caret=(6, 9)' 20 && ok "a click on it goes to line 6, column 9" || bad "jump: $(state)"

echo "=== clangd"
if V "jexec ws_alice_proj test -x /atrium/toolchains/c-dev/bin/clangd"; then
    waitstate 'lsp=Some("clangd") diags=[2-9]' 120 && ok "clangd answers, with its own diagnostic" || bad "clangd: $(state)"
else
    echo "  (no clangd in proj: the c-dev layer isn't installed here; skipped)"
fi

echo "=== rust-analyzer"
if [ -n "$RUST" ]; then
    rows() { state | sed 's/.*rows=\[\([^]]*\)\].*/\1/' | tr ',' '\n'; }
    ROW=$(rows | grep -n '^rs$' | cut -d: -f1)
    press 100 $((89 + 22 * (${ROW:-1} - 1)))
    waitstate 'rows=\[[^]]*rs/main.rs' 20 && ok "a click opens the rs folder" || bad "rs: $(state)"
    ROW=$(rows | grep -n '^rs/main.rs$' | cut -d: -f1)
    press 100 $((89 + 22 * (${ROW:-1} - 1)))
    waitstate 'tabs=\[hello.c,rs/main.rs\]' 20 && ok "and main.rs opens" || bad "main.rs: $(state)"
    # Its first answer indexes std under TCG: minutes.
    waitstate 'lsp=Some("rust-analyzer") diags=[1-9]' 1500 && ok "rust-analyzer answers, with a diagnostic ($(state | sed 's/.*\(lsp=.*diags=[0-9]*\).*/\1/'))" || bad "rust-analyzer: $(state)"
    shot artifex-5-rust
else
    echo "  (RUST not set: skipped)"
fi

echo "=== composition and Run"
if V "test -f $W/proj/work/app/Makefile"; then
    rows() { state | sed 's/.*rows=\[\([^]]*\)\].*/\1/' | tr ',' '\n'; }
    ROW=$(rows | grep -n '^app$' | cut -d: -f1)
    press 100 $((89 + 22 * (${ROW:-1} - 1)))
    waitstate 'rows=\[[^]]*app/app.c' 20 || bad "app: $(state)"
    ROW=$(rows | grep -n '^app/app.c$' | cut -d: -f1)
    press 100 $((89 + 22 * (${ROW:-1} - 1)))
    waitstate 'tabs=\[[^]]*app/app.c\]' 20 && ok "app.c of a c-app project opens" || bad "app.c: $(state)"
    waitstate 'comp=Some(("org.example.app", 0,' 20 && ok "the composition panel has its core (org.example.app, no workers)" || bad "composition: $(state)"
    if V "jexec ws_alice_proj test -x /atrium/toolchains/rust/bin/cargo-atrium"; then
        waitstate 'comp=Some(("org.example.app", 0, true, true))' 300 && ok "cargo atrium check answers for it" || bad "check: $(state)"
    fi
    ctrl 0x15                  # Ctrl+R: the project's Run task (make run)
    waitstate 'ask=Some("app")' 300 && ok "make run builds it and asks to Run app in Artifex" || bad "ask: $(state) $(V "tail -5 $B/artifex.log")"
    shot artifex-6-ask
    # The question's Run button (ui.rs: over the output, right-aligned).
    GAP=8 press 980 520
    DEV=dev.alice.proj.org.example.app
    # Under TCG a busy guest (rust-analyzer still indexing) can take more
    # than the press's 1 s lifetime to reach frescod: ask once more then.
    n=0
    while [ $n -lt 3 ] && ! waitstate "note=Some(\"Running $DEV\")" 120 \
        && [ "$(V "grep -c 'proj/app for alice REFUSED: the press was too long ago' $LOG")" -gt $n ]; do
        n=$((n + 1)); echo "  (the press expired on the way: again, $n)"
        waitstate 'running=None' 120
        ctrl 0x15
        waitstate 'ask=Some("app")' 300 && GAP=8 press 980 520
    done
    waitstate "note=Some(\"Running $DEV\")" 120 && ok "a press on Run starts it as a dev app ($DEV)" || bad "run: $(state) $(V "grep -i 'Run\|REFUSED' $LOG | tail -3")"
    waitstate 'running=None' 60 && ok "and make run ends" || bad "make run still running: $(state)"
    sleep 15
    V "grep -q 'no window' $LOG" && bad "the app has no window: $(V "grep -A2 'atrium_init' $LOG | tail -3")"
    V "grep -q '$DEV (a Run) exited' $LOG" && bad "it ended: $(V "grep '$DEV (a Run) exited' $LOG")" || ok "its window stays up"
    shot artifex-7-dev-app
else
    echo "  (no C SDK in the base (install-c-sdk.sh): skipped)"
fi

# The dev app took the focus, and frescod alone (no Forum) doesn't move
# it on a click: close it, and click Artifex.
V "for j in \$(jls name | grep '^dev_alice_'); do jail -r \$j; done" >/dev/null 2>&1
GAP=8 press 700 640

echo "=== Debug a dev app (lldb-dap from the c-dev layer)"
# F5 on app (an atrium.toml) asks to Debug it; the press sends
# WorkspaceDebug, and lldb-dap in proj attaches over gdb-remote to the build
# portcullisd started under lldb-server, in its own jail.
if V "test -f $W/proj/work/app/Makefile" && V "jexec ws_alice_proj test -x /atrium/toolchains/c-dev/bin/lldb-dap"; then
    ROW=$(rows | grep -n '^app/app.c$' | cut -d: -f1)
    press 100 $((89 + 22 * (${ROW:-1} - 1)))
    waitstate 'tabs=\[[^]]*app/app.c\]' 20 || bad "app.c: $(state)"
    # The caret to line 32 (`float span = ...`, run every frame), and F9.
    press 700 200
    i=0; while [ $i -lt 40 ]; do inj "KEY 0x4a 1 1" "KEY 0x4a 0 1"; state | grep -q 'caret=(1,' && break; sleep 1; i=$((i+1)); done
    keys=""; for n in $(seq 1 31); do keys="$keys \"KEY 0x51 1\" \"KEY 0x51 0\""; done
    eval inj $keys; sleep 4
    waitstate 'caret=(32,' 30 || bad "caret: $(state)"
    inj "KEY 0x42 1" "KEY 0x42 0"; sleep 4
    waitstate 'bps={"app/app.c": \[31\]}' 20 && ok "F9 sets a breakpoint on app.c:32" || bad "breakpoint: $(state)"
    inj "KEY 0x3e 1" "KEY 0x3e 0"
    waitstate 'ask=Some("app")' 300 && ok "F5 builds app and asks to Debug it" || bad "debug ask: $(state) $(V "tail -5 $B/artifex.log")"
    shot artifex-8-debug-ask
    GAP=8 press 980 520
    DEV=dev.alice.proj.org.example.app
    waitstate 'dbg=Some(("Stopped (breakpoint)", true, Some("app/app.c:3' 300 \
        && ok "the press starts it and lldb-dap stops it at the breakpoint ($(state | grep -o 'app/app.c:[0-9]*' | tail -1))" || bad "debug: $(state) $(V "grep -i 'debug\|REFUSED' $LOG | tail -4")"
    check "it runs in its own jail as the dev build" "1" "$(V "jls name | grep -c '^dev_alice_proj'")"
    check "marked as debugging (the frame tag)" "$DEV" "$(V "ls /var/db/atrium/debugging 2>/dev/null")"
    shot artifex-9-debug-stopped
    inj "KEY 0x3e 1" "KEY 0x3e 0"   # F5: continue, to the next frame
    waitstate 'dbg=Some(("Stopped (breakpoint)", true, Some("app/app.c:3' 120 && ok "continue stops at it again (the next frame)" || bad "continue: $(state)"
    V "for j in \$(jls name | grep '^dev_alice_'); do jail -r \$j; done" >/dev/null 2>&1
    sleep 10
    GAP=8 press 700 640
else
    echo "  (no app project, or no lldb-dap in proj (CDEV= a c-dev layer with it): skipped)"
fi

echo "=== New project"
if V "jexec ws_alice_proj test -x /atrium/toolchains/rust/bin/cargo-atrium"; then
    press 962 52               # the bar's New project (ui.rs)
    waitstate 'templates=[2-9]' 300 && ok "New project lists the SDK's templates ($(state | sed 's/.*templates=\([0-9]*\).*/\1/'))" || bad "templates: $(state)"
    shot artifex-11-new-project
    # The form (ui.rs: 460 wide, centred; rows 40 apart from 72 down): c-app,
    # the name field, Create.
    N=$(state | sed 's/.*templates=\([0-9]*\).*/\1/')
    CY=$(( (660 - 220 - 40 * N) / 2 + 30 ))
    press 590 $((CY + 72 + 40 + 18))
    press 590 $((CY + 72 + 40 * N + 12 + 14 + 16))
    typ "made"
    press 650 $((CY + 72 + 40 * N + 12 + 14 + 52 + 16))
    waitstate 'rows=\[[^]]*made' 120 && ok "Create makes made/ from c-app" || bad "create: $(state)"
    check "with its Makefile and made.c" "1" "$(V "test -f $W/proj/work/made/Makefile -a -f $W/proj/work/made/made.c && echo 1")"
else
    echo "  (no cargo-atrium in proj: skipped)"
fi

echo "=== fuzz and Move into a worker"
if V "test -f $W/proj/work/photos/Cargo.toml && jexec ws_alice_proj /atrium/toolchains/rust/bin/cargo-atrium atrium 2>&1 | grep -q 'worker add'"; then
    rows() { state | sed 's/.*rows=\[\([^]]*\)\].*/\1/' | tr ',' '\n'; }
    for r in photos photos/src photos/src/lib.rs; do
        ROW=$(rows | grep -n "^$r\$" | cut -d: -f1)
        [ -n "$ROW" ] || { bad "no row $r: $(state)"; break; }
        press 100 $((89 + 22 * (ROW - 1)))
    done
    waitstate 'tabs=\[[^]]*photos/src/lib.rs' 20 && ok "photos/src/lib.rs opens" || bad "lib.rs: $(state)"
    waitstate 'fuzz=Some((false, "core:album", 0' 300 && ok "the fuzz panel lists core:album" || bad "fuzz list: $(state)"
    # Fuzz tab, then Start fuzzing (ui.rs: tabs from the panel's left, the
    # button 220 from the right).
    press 622 485
    press 985 485
    # libFuzzer and the harness build in the workspace under TCG: slow.
    waitstate 'fuzz=Some((true, "core:album", [1-9][0-9]*, true' 3600 && ok "fuzzing finds the crash in the core" || bad "fuzz: $(state) $(V "tail -5 $B/artifex.log")"
    shot artifex-12-fuzz-crash
    press 985 485              # Stop fuzzing
    # The fix line's Move it button (row 2 of the panel, right-aligned).
    press 1072 544
    waitstate 'moving=true' 300 && ok "Move it previews cargo atrium worker add" || bad "preview: $(state)"
    shot artifex-13-move-preview
    check "the preview writes nothing" "0" "$(V "grep -c 'workers' $W/proj/work/photos/atrium.toml")"
    press 980 520              # the preview's Move
    waitstate 'note=Some("Moved crate::read_album into the worker read-album")' 300 && ok "Move makes the worker" || bad "move: $(state)"
    check "atrium.toml has it" "1" "$(V "grep -c '^\[\[workers\]\]' $W/proj/work/photos/atrium.toml")"
    waitstate 'comp=Some(("org.example.photos", 1,' 60 && ok "the composition panel shows the worker" || bad "composition: $(state)"
    waitstate 'fuzz=Some((false, "[^"]*worker:read-album' 300 && ok "and the fuzz panel has worker:read-album" || bad "fuzz targets: $(state)"
    shot artifex-14-moved
else
    echo "  (no vendored fuzz crates (FUZZ_VENDOR), or no cargo-atrium with worker add in the Rust layer: skipped)"
fi

echo "=== terminal"
press 1080 52              # the bar's Terminal button
i=0; while [ $i -lt 30 ] && ! V "grep -q 'console in workspace/proj' $LOG"; do sleep 2; i=$((i+2)); done
V "grep -q 'console in workspace/proj' $LOG" && ok "Terminal opens proj's terminal" || bad "terminal: $(V "grep -i console $LOG | tail -2")"
shot artifex-4-terminal

echo "=== under Forum"
if [ -n "${FORUM:-}" ]; then
    # Forum's WM core over the same frescod: it places every window by role
    # (forum.md §2). The dev app is started again, from the person's CLI.
    V "for j in \$(jls name | grep '^dev_alice_'); do jail -r \$j; done" >/dev/null 2>&1
    scp -q $K -P 2222 "$ROOT/forum-wm/target/$TARGET/release/forum-wm" root@localhost:$B/
    V "cd $B && env FRESCO_SOCKET=/atrium/sockets/fresco/fresco.sock FRESCO_WM_SOCKET=$B/wm.sock FORUM_CTL_SOCKET=$B/ctl.sock \
       FORUM_STATUS_SOCKET=$B/status.sock FORUM_STATE=$B/forum-state daemon -f -o $B/wm.log $B/forum-wm </dev/null"
    sleep 20
    V "pgrep -x forum-wm >/dev/null" && ok "forum-wm is up" || bad "forum-wm: $(V "tail -5 $B/wm.log")"
    shot artifex-8-forum
    n=$(V "wc -l < $LOG" | tr -d ' ')
    out=$(V "su -m alice -c 'portcullis workspace run proj app'" 2>&1)
    echo "$out" | grep -q '^dev.alice.proj.org.example.app' && ok "Run again from the CLI" || bad "cli run: $out"
    sleep 25
    V "tail -n +$((n+1)) $LOG | grep -q 'dev.alice.proj.org.example.app (a Run) exited\|no window'" && bad "it ended: $(V "tail -3 $LOG")" || ok "the dev app runs under Forum"
    shot artifex-9-forum-dev-app
    V "tail -15 $B/wm.log" | sed 's/^/  wm: /'
else
    echo "  (FORUM not set: skipped)"
fi

echo "artifex: $fails failed"
[ $fails = 0 ]
