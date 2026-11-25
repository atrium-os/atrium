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
# SKIP="newproject fuzz assets": leave out those sections (to iterate on
# another; a run that skips any is not a pass of the whole check).
want() { case " ${SKIP:-} " in *" $1 "*) echo "  (skipped: SKIP has $1)"; return 1;; esac; }
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
xb atrium-image-decode ""
[ -z "${FORUM:-}" ] || xb forum-wm "--bin forum-wm"
# The Inspect section's dev app: the SDK's app template as `cargo atrium new`
# makes it, built as a dev build (debug assertions on, so Pergola records
# source lines and answers the inspector), against this tree's Pergola. In
# scratch/ (never committed): inside the tree, where the toolchain pin and
# the cross config apply.
UI=$ROOT/scratch/artifex-ui-app
if want inspect; then
    rm -rf "$UI"; mkdir -p "$UI/src"
    sed 's/{{name}}/ui/g; s/{{title}}/UI/g' "$ROOT/sdk/templates/app/src/main.rs" > "$UI/src/main.rs"
    sed 's/{{name}}/ui/g; s/{{title}}/UI/g; s/{{id}}/org.example.ui/g' "$ROOT/sdk/templates/app/atrium.toml.tmpl" > "$UI/atrium.toml"
    printf '[package]\nname = "ui"\nversion = "0.1.0"\nedition = "2021"\n\n[[bin]]\nname = "ui"\npath = "src/main.rs"\n\n[dependencies]\npergola = { path = "%s/pergola" }\n\n[workspace]\n' "$ROOT" > "$UI/Cargo.toml"
    cp "$ROOT/pergola/Cargo.lock" "$UI/" 2>/dev/null
    (cd "$UI" && RUSTFLAGS="$CROSS_RUSTFLAGS" CARGO_PROFILE_DEV_DEBUG=0 cargo build -q --target "$TARGET") \
        || { echo "build of the inspector's dev app failed"; echo "artifex: 1 failed"; exit 1; }
fi

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
# The system's image decoder, for the Assets view's previews.
for f in atrium-image-decode/target/$TARGET/release/atrium-image-decode scripts/image-decode-bundle.sh etc/jaild.policy.toml; do
    scp -q $K -P 2222 "$ROOT/$f" root@localhost:$B/ || { echo "copy $f failed"; echo "artifex: 1 failed"; exit 1; }
done
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
# A dev VM's app data is in no key group: portcullisd launches apps only
# with plain_app_data (mail.md §6.4a A1), a top-level key, so it goes first.
VP=/etc/atrium/volumes.policy.toml
if ! grep -qE '^[[:space:]]*plain_app_data[[:space:]]*=' \$VP 2>/dev/null; then
  { echo 'plain_app_data = true'; echo; cat \$VP 2>/dev/null; } > \$VP.new && mv \$VP.new \$VP
  service atrium-volumes restart >/dev/null 2>&1 || true
fi
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
# The image decoder (vm-image-decode-check.sh installs it the same way):
# jaild's policy with its uid and rctl on (the bundle has [resources]).
cp $B/jaild.policy.toml $B/jaild.policy.new
printf '\n[resource_control]\nallow_rctl = true\n' >> $B/jaild.policy.new
cmp -s $B/jaild.policy.new /etc/atrium/jaild.policy.toml || { cp $B/jaild.policy.new /etc/atrium/jaild.policy.toml; service atrium-jaild restart >/dev/null 2>&1 || true; }
(cd $B && sh image-decode-bundle.sh $B/atrium-image-decode /root/of-key.pem /usr/local/bin/opifex >/dev/null)
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
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_TOKEN_LIFE_MS=${TOKEN_LIFE_MS:-15000} FRESCOD_ACTIVATION_UIDS=0 \
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
V "rm -rf $B/ui-app"
if want inspect; then
    V "mkdir -p $B/ui-app" && scp -q $K -P 2222 "$UI/target/$TARGET/debug/ui" "$UI/src/main.rs" "$UI/atrium.toml" root@localhost:$B/ui-app/ \
        || echo "  (the inspector's dev app didn't copy)"
fi
# The make rules from this tree: a base older than 2a7b458a7 builds with no
# -g (sys.mk's CFLAGS won over a ?=), and Debug then finds no lines.
scp -q $K -P 2222 "$ROOT/etc/mk/atrium.app.mk" root@localhost:$B/atrium.app.mk \
    && V "test ! -f /usr/share/mk/atrium.app.mk || install -m 0444 $B/atrium.app.mk /usr/share/mk/atrium.app.mk"
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
    # A workspace keeps its own copy of /usr/share from when it was made.
    for m in $W/*/.state/root/usr/share/mk; do [ -d \$m ] && install -m 0444 /usr/share/mk/atrium.app.mk \$m/; done
    mkdir -p $W/proj/work/app
    T=$B/c-app
    sed 's/{{name}}/app/g' \$T/Makefile.tmpl > $W/proj/work/app/Makefile
    sed 's/{{name}}/app/g; s/{{title}}/App/g; s/{{id}}/org.example.app/g' \$T/atrium.toml.tmpl > $W/proj/work/app/atrium.toml
    # Line 9 (blank in the template) becomes a function line 32 calls, so
    # the Debug section's Into and Out have somewhere to go.
    sed 's/{{name}}/app/g; s/{{title}}/App/g; 9s/^$/static float span_of(float w) { return w > 160 ? w - 160 : 1; }/; s/float span = width > 160 ? width - 160 : 1;/float span = span_of(width);/' \$T/main.c.tmpl > $W/proj/work/app/app.c
fi
# A Pergola app from the app template, built on the host as a dev build,
# with `make run` to Run it: the Inspect section.
if [ -f $B/ui-app/ui ]; then
    U=$W/proj/work/ui
    mkdir -p \$U/src \$U/bin
    install -m 0755 $B/ui-app/ui \$U/bin/ui
    cp $B/ui-app/main.rs \$U/src/main.rs
    cp $B/ui-app/atrium.toml \$U/atrium.toml
    printf 'run:\n\tatrium-run ui\n' > \$U/Makefile
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
    # As for Run: a press that reached frescod too late is asked again.
    n=0
    while [ $n -lt 3 ] && waitstate 'dbg=Some(("Stopped (breakpoint)", true, Some("app/app.c:3\|press was too long ago' 300 \
        && state | grep -q 'press was too long ago'; do
        n=$((n + 1)); echo "  (the press expired on the way: again, $n)"
        inj "KEY 0x3e 1" "KEY 0x3e 0"
        waitstate 'ask=Some("app")' 300 && GAP=8 press 980 520
    done
    waitstate 'dbg=Some(("Stopped (breakpoint)", true, Some("app/app.c:3' 300 \
        && ok "the press starts it and lldb-dap stops it at the breakpoint ($(state | grep -o 'app/app.c:[0-9]*' | tail -1))" || bad "debug: $(state) $(V "grep -i 'debug\|REFUSED' $LOG | tail -4")"
    check "it runs in its own jail as the dev build" "1" "$(V "jls name | grep -c '^dev_alice_proj'")"
    check "Debug built it at -O0 (make ATRIUM_DEBUG=1)" "1" "$(V "grep -c -- ' -O0' $W/proj/work/app/.atrium-cflags")"
    check "marked as debugging (the frame tag)" "$DEV" "$(V "ls /var/db/atrium/debugging 2>/dev/null")"
    shot artifex-9-debug-stopped
    # Stepping: Into span_of (line 9), Out back to 32, Over to 33.
    # The toolbar, not F-keys: with frescod alone (no Forum) the app's
    # window keeps the keyboard once it exists, but a press still lands
    # on the window under it. Continue 560, Over 650, Into 724, Out 794.
    press 724 52
    waitstate 'dbg=Some(("Stopped (step)", true, Some("app/app.c:9"' 120 && ok "Into steps into span_of" || bad "into: $(state)"
    shot artifex-10-debug-into
    # Out lands in main on line 32, or on 33 when -O2 left nothing of 32
    # after the call; Over then moves to a later line of main.
    press 794 52
    waitstate 'dbg=Some(("Stopped (step)", true, Some("app/app.c:3[23]"' 120 && ok "Out returns to main" || bad "out: $(state)"
    L=$(state | sed -n 's/.*dbg=Some(("Stopped (step)", true, Some("app\/app.c:\([0-9]*\)".*/\1/p')
    case "$L" in ''|*[!0-9]*) L=32;; esac
    press 650 52
    waitstate 'dbg=Some(("Stopped (step)", true, Some("app/app.c:3[3-8]"' 120 \
        && [ "$(state | sed -n 's/.*dbg=Some(("Stopped (step)", true, Some("app\/app.c:\([0-9]*\)".*/\1/p')" -gt "$L" ] \
        && ok "Over goes on to a later line of main" || bad "over (from $L): $(state)"
    press 560 52   # Continue, to the next frame
    waitstate 'dbg=Some(("Stopped (breakpoint)", true, Some("app/app.c:3' 120 && ok "continue stops at it again (the next frame)" || bad "continue: $(state)"
    press 962 52   # Stop: ends the program and the session
    i=0; while [ $i -lt 30 ] && [ "$(V "pgrep -x lldb-dap | wc -l; jls name | grep -c '^dev_alice_'" | tr -d ' \n')" != 00 ]; do sleep 2; i=$((i+1)); done
    check "Stop ends lldb-dap" "0" "$(V "pgrep -x lldb-dap | wc -l" | tr -d ' ')"
    check "and the app's jail with it" "0" "$(V "jls name | grep -c '^dev_alice_'")"
    V "for j in \$(jls name | grep '^dev_alice_'); do jail -r \$j; done" >/dev/null 2>&1
    GAP=8 press 700 640
else
    echo "  (no app project, or no lldb-dap in proj (CDEV= a c-dev layer with it): skipped)"
fi

echo "=== Inspect a dev app (a Pergola dev build)"
# Run from Artifex hands it the inspector's socket; the app (the app
# template, a dev build) answers with its tree. Pick takes a press in the
# app's window; the node's line is the template's own; Go to source opens it.
if want inspect && V "test -x $W/proj/work/ui/bin/ui"; then
    rows() { state | sed 's/.*rows=\[\([^]]*\)\].*/\1/' | tr ',' '\n'; }
    for r in ui ui/src ui/src/main.rs; do
        ROW=$(rows | grep -n "^$r\$" | cut -d: -f1)
        [ -n "$ROW" ] && press 100 $((89 + 22 * (ROW - 1)))
    done
    waitstate 'tabs=\[[^]]*ui/src/main.rs' 20 && ok "ui/src/main.rs of the app template opens" || bad "main.rs: $(state)"
    # Run on the toolbar (make run, which asks to Run ui): a key would go
    # to no one, as frescod alone gives the focus back to no window when
    # the debugged app's closes.
    press 296 52
    waitstate 'ask=Some("ui")' 300 && ok "make run asks to Run ui" || bad "ask: $(state)"
    GAP=8 press 980 520
    UIDEV=dev.alice.proj.org.example.ui
    n=0
    while [ $n -lt 3 ] && ! waitstate "note=Some(\"Running $UIDEV\")" 120 \
        && [ "$(V "grep -c 'proj/ui for alice REFUSED: the press was too long ago' $LOG")" -gt $n ]; do
        n=$((n + 1)); echo "  (the press expired on the way: again, $n)"
        waitstate 'running=None' 120
        press 296 52
        waitstate 'ask=Some("ui")' 300 && GAP=8 press 980 520
    done
    waitstate "note=Some(\"Running $UIDEV\")" 120 && ok "the press runs it with the inspector ($UIDEV)" || bad "run: $(state) $(V "grep -i 'Inspect\|REFUSED' $LOG | tail -3")"
    V "grep -q '$UIDEV starts with the inspector at fd 3' $LOG" && ok "portcullisd puts the inspector's socket at its fd 3" || bad "no socket: $(V "grep '$UIDEV' $LOG | tail -3")"
    waitstate "inspect=Some((\"$UIDEV\", true, true, [1-9]" 300 && ok "a Pergola dev build answers with its tree ($(state | sed 's/.*inspect=Some(([^,]*, true, true, \([0-9]*\).*/\1/') nodes)" \
        || bad "no tree: $(state | sed 's/.*inspect=//')"
    shot artifex-22-inspect-app
    # The strip's Inspect entry: last, 42 apart from Files at 100.
    I=$(state | sed 's/.*inspect=Some(.*, \([0-9]*\)))$/\1/')
    case "$I" in ''|*[!0-9]*) I=5;; esac
    press 64 $((100 + 42 * ${I:-5}))
    waitstate 'view=Inspect' 20 && ok "the strip's Inspect shows the tree" || bad "view: $(state)"
    # Pick (views.rs: right of the view's header), then a press on the
    # app's button, in its window at the screen's top-left. (The window
    # has no background: a press between glyphs of its text goes through
    # to Artifex under it.)
    press 864 102
    waitstate "inspect=Some((\"$UIDEV\", true, true, [0-9]*, true," 30 && ok "Pick waits for a press in the app" || bad "pick: $(state | sed 's/.*inspect=//')"
    press 84 88
    LT=$(grep -n 'Button::primary("Press")' "$ROOT/sdk/templates/app/src/main.rs" | head -1 | cut -d: -f1)
    waitstate "inspect=Some((\"$UIDEV\", true, true, [0-9]*, false, false, Some(\"[A-Za-z]*\"), Some(\"ui/src/main.rs:$LT\")" 60 \
        && ok "the press picks its button, drawn at ui/src/main.rs:$LT ($(state | sed 's/.*inspect=Some(([^,]*, [^,]*, [^,]*, [^,]*, [^,]*, [^,]*, Some("\([^"]*\)").*/\1/'))" || bad "picked: $(state | sed 's/.*inspect=//')"
    shot artifex-23-inspect-picked
    press 974 103              # Bounds
    waitstate "inspect=Some((\"$UIDEV\", true, true, [0-9]*, false, true," 30 && ok "Bounds outlines every box in the app" || bad "bounds: $(state | sed 's/.*inspect=//')"
    shot artifex-24-inspect-bounds
    press 1050 158             # Go to source
    waitstate "tabs=\\[[^]]*ui/src/main.rs[^]]*\\] active=[^ ]* caret=($LT, 1)" 20 && ok "Go to source opens main.rs at line $LT" || bad "source: $(state)"
    V "for j in \$(jls name | grep '^dev_alice_'); do jail -r \$j; done" >/dev/null 2>&1
    waitstate "inspect=Some((\"$UIDEV\", false, true," 60 && ok "when the app ends its last tree stays, marked ended" || bad "ended: $(state | sed 's/.*inspect=//')"
    GAP=8 press 700 640
else
    echo "  (no dev app for the inspector: skipped)"
fi

echo "=== New project"
if want newproject && V "jexec ws_alice_proj test -x /atrium/toolchains/rust/bin/cargo-atrium"; then
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
if want fuzz && V "test -f $W/proj/work/photos/Cargo.toml && jexec ws_alice_proj /atrium/toolchains/rust/bin/cargo-atrium atrium 2>&1 | grep -q 'worker add'"; then
    rows() { state | sed 's/.*rows=\[\([^]]*\)\].*/\1/' | tr ',' '\n'; }
    # New project leaves its Composition view up: the strip's Files (the
    # first, 8 under the toolbar) brings back the tree.
    press 64 100
    waitstate 'view=Code' 20 || bad "files: $(state)"
    for r in photos photos/src photos/src/lib.rs; do
        ROW=$(rows | grep -n "^$r\$" | cut -d: -f1)
        [ -n "$ROW" ] || { bad "no row $r: $(state)"; break; }
        press 100 $((89 + 22 * (ROW - 1)))
    done
    waitstate 'tabs=\[[^]]*photos/src/lib.rs' 20 && ok "photos/src/lib.rs opens" || bad "lib.rs: $(state)"
    waitstate 'fuzz=Some((false, "core:album", 0' 300 && ok "the fuzz panel lists core:album" || bad "fuzz list: $(state)"
    # The view strip's Fuzz (ui.rs: Files, Problems, Composition, Fuzz, 42
    # apart from 8 under the toolbar), then Start fuzzing (views.rs: at the
    # top right of the 300-wide list).
    press 64 226
    waitstate 'view=Fuzz' 20 && ok "the view strip opens the Fuzz view" || bad "strip: $(state)"
    press 309 100
    # libFuzzer and the harness build in the workspace under TCG: slow.
    waitstate 'fuzz=Some((true, "core:album", [1-9][0-9]*, true' 3600 && ok "fuzzing finds the crash in the core" || bad "fuzz: $(state) $(V "tail -5 $B/artifex.log")"
    shot artifex-12-fuzz-crash
    press 309 100              # Stop fuzzing
    # The crash's Move into a worker (views.rs: first under its summary).
    press 498 222
    waitstate 'moving=true' 300 && ok "Move it previews cargo atrium worker add" || bad "preview: $(state)"
    shot artifex-13-move-preview
    check "the preview writes nothing" "0" "$(V "grep -c 'workers' $W/proj/work/photos/atrium.toml")"
    press 1076 636             # the preview pane's Move (bottom right)
    waitstate 'note=Some("Moved crate::read_album into the worker read-album")' 300 && ok "Move makes the worker" || bad "move: $(state)"
    check "atrium.toml has it" "1" "$(V "grep -c '^\[\[workers\]\]' $W/proj/work/photos/atrium.toml")"
    waitstate 'comp=Some(("org.example.photos", 1,' 60 && ok "the composition panel shows the worker" || bad "composition: $(state)"
    waitstate 'fuzz=Some((false, "[^"]*worker:read-album' 300 && ok "and the fuzz panel has worker:read-album" || bad "fuzz targets: $(state)"
    shot artifex-14-moved
    press 64 184               # the strip's Composition
    waitstate 'view=Composition' 20 && ok "the strip opens the Composition view" || bad "composition view: $(state)"
    shot artifex-15-composition
else
    echo "  (no vendored fuzz crates (FUZZ_VENDOR), or no cargo-atrium with worker add in the Rust layer: skipped)"
fi

echo "=== Assets"
# The Assets view (dev-tools.md §3.5, step 4): the project's assets/, an
# icon at four sizes and a picture decoded by the system's decoder (never
# in Artifex), where the icon is used, the dock's check of it (cargo
# atrium icon check) and Import SVG (cargo atrium icon import).
P=; F=
if V "test -f $W/proj/work/photos/atrium.toml"; then P=photos; F=photos/Cargo.toml
elif V "test -f $W/proj/work/app/atrium.toml"; then P=app; F=app/app.c; fi
if want assets && [ -n "$P" ]; then
    rows() { state | sed 's/.*rows=\[\([^]]*\)\].*/\1/' | tr ',' '\n'; }
    FX=$(mktemp -d)
    python3 -I - "$FX" <<'PY'
import struct, sys, zlib
d = sys.argv[1]
def png(w, h, px):
    raw = b"".join(b"\0" + b"".join(bytes(px(x, y)) for x in range(w)) for y in range(h))
    chunk = lambda t, b: struct.pack(">I", len(b)) + t + b + struct.pack(">I", zlib.crc32(t + b) & 0xffffffff)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(d + "/photo.png", "wb").write(png(300, 180, lambda x, y: (0x30 + x // 3, 0x90, 0x40 + y, 0xff)))
open(d + "/app.svg", "w").write('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="2" y="2" width="20" height="20" rx="5" fill="#c06030"/><circle cx="12" cy="12" r="5" fill="#ffffff"/></svg>')
open(d + "/Logo.svg", "w").write('<svg xmlns="http://www.w3.org/2000/svg" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" width="48" height="48" viewBox="0 0 48 48"><metadata>drawn elsewhere</metadata><g inkscape:label="Layer 1" transform="translate(4,4)"><rect width="40" height="40" rx="8" fill="#3060c0"/><path d="M10 30 L20 10 L30 30 Z" fill="#ffd040" style="fill-opacity:0.9"/></g><text x="4" y="46">Logo</text></svg>')
open(d + "/ui.rs", "w").write('// The window\'s icon.\npub fn icon() -> Option<atrium_assets::Picture> {\n    atrium_assets::image("icons/app.svg", 48, 48).ok()\n}\n')
PY
    for f in photo.png app.svg Logo.svg ui.rs; do scp -q $K -P 2222 "$FX/$f" root@localhost:$B/fx-$f; done
    rm -rf "$FX"
    V "Q=$W/proj/work/$P; mkdir -p \$Q/assets/icons \$Q/art \$Q/src; cp $B/fx-photo.png \$Q/assets/photo.png; cp $B/fx-app.svg \$Q/assets/icons/app.svg; \
       cp $B/fx-Logo.svg \$Q/art/Logo.svg; cp $B/fx-ui.rs \$Q/src/ui.rs; rm -f \$Q/assets/icons/logo.svg; chown -R alice:files-alice \$Q; chmod -R g+rwX \$Q"
    # Into the project (a file of it), then the strip's Assets: after Fuzz
    # when the project has fuzz targets listed (ui.rs, 42 apart).
    press 64 100
    waitstate 'view=Code' 20 || bad "files: $(state)"
    # The project's folder opens (unless it is open already), then its file.
    for r in $P $F; do
        [ "$r" = "$P" ] && rows | grep -q "^$P/" && continue
        ROW=$(rows | grep -n "^$r\$" | cut -d: -f1)
        [ -n "$ROW" ] || { bad "no row $r: $(state)"; break; }
        press 100 $((89 + 22 * (ROW - 1)))
    done
    waitstate "tabs=\[[^]]*$F" 20 || bad "open $F: $(state)"
    if state | grep -q 'fuzz=Some'; then AY=268; else AY=226; fi
    press 64 $AY
    waitstate 'view=Assets' 20 && ok "the strip opens the Assets view" || bad "assets view: $(state)"
    waitstate 'assets=Some((2, "assets/icons/app.svg", 4, Some(1)' 300 \
        && ok "the icon is decoded at 16, 24, 32 and 48 by the system's decoder, and src/ui.rs uses it" || bad "icon preview: $(state) $(V "grep -i 'system worker\|image-decode' $LOG | tail -3")"
    ICON=; V "jexec ws_alice_proj /atrium/toolchains/rust/bin/cargo-atrium atrium help 2>&1 | grep -q 'icon import'" && ICON=1
    if [ -n "$ICON" ]; then
        waitstate 'assets=Some((2, "assets/icons/app.svg", 4, Some(1), Some(true)' 300 && ok "cargo atrium icon check: the dock draws it" || bad "icon check: $(state)"
    fi
    V "grep -q 'system worker org.atrium.image-decode .* for $ART .*runs as _imagedec' $LOG" && ok "the decoder ran as _imagedec for Artifex" || bad "decoder: $(V "grep -i 'system worker' $LOG | tail -3")"
    shot artifex-16-assets-icon
    # The picture: the list's second row (views.rs: under PICTURES).
    press 200 211
    waitstate 'assets=Some((2, "assets/photo.png", 1, Some(0)' 120 && ok "the picture is decoded, fitted, and nothing uses it yet" || bad "picture: $(state)"
    shot artifex-17-assets-picture
    if [ -n "$ICON" ]; then
        press 314 100            # Import SVG (the list's top right)
        waitstate '"choosing 1")' 30 && ok "Import SVG lists the project's SVG outside assets/" || bad "choose: $(state)"
        press 512 217            # its row
        waitstate '"planned assets/icons/logo.svg ok=true dropped=[1-9][0-9]* preview=4")' 300 \
            && ok "the preview converts it, names what it drops, and is decoded" || bad "import preview: $(state)"
        check "the preview writes nothing" "0" "$(V "test -e $W/proj/work/$P/assets/icons/logo.svg && echo 1 || echo 0")"
        shot artifex-18-import-preview
        press 467 630            # Import
        waitstate 'note=Some("Imported art/Logo.svg as assets/icons/logo.svg")' 120 && ok "Import writes assets/icons/logo.svg" || bad "import: $(state)"
        waitstate 'assets=Some((3, "assets/icons/logo.svg", 4' 120 && ok "and shows it, decoded" || bad "imported: $(state)"
        shot artifex-19-imported
        # The icon editor (step 5): New icon, shapes on the 24 grid, a
        # gradient and a stroke, the pen, reordering, Save; the saved icon
        # then goes through the system's decoder and the dock's check like
        # any other. In the 1100x660 window the canvas is 288 pt at window
        # (372, 142): a unit is 12 pt, so unit (u, v) is screen
        # (412 + 12u, 172 + 12v); the fill and stroke start at window x 684,
        # the shape list at 916 (views.rs icon_editor; screen = window +
        # (40, 30)).
        u() { echo $((412 + 12 * $1)) $((172 + 12 * $2)); }
        drag() { inj "MOVE $1 $2" "BTN 1 1" "MOVE $(( ($1 + $3) / 2 )) $(( ($2 + $4) / 2 ))" "MOVE $3 $4" "BTN 1 0"; sleep ${GAP:-4}; }
        IC='icon=Some(("assets/icons/icon.svg"'
        press 1058 110           # New icon (the right pane's top right)
        waitstate "$IC, Select, 0," 20 && ok "New icon opens the editor on assets/icons/icon.svg" || bad "new icon: $(state)"
        press 535 145            # Rectangle
        drag $(u 2 2) $(u 22 22)
        press 908 202            # Fill: Radial
        waitstate "$IC, Rect, 1, \"r\", \"r\"" 20 && ok "a rectangle drawn on the grid, filled with a radial gradient" || bad "rect: $(state)"
        press 619 145            # Ellipse, then (it is selected) Fill: Linear
        drag $(u 7 7) $(u 17 17)
        press 855 202
        press 703 145            # Line, then a stroke 2 wide
        drag $(u 5 19) $(u 19 5)
        press 869 420
        press 787 145            # Pen: three points, then the first again
        for pt in "4 4" "9 4" "4 9" "4 4"; do press $(u $pt); done
        waitstate "$IC, Pen, 4, \"relp\", \"rlll\", 0, true" 30 && ok "an ellipse, a stroked line and a closed pen shape" || bad "shapes: $(state)"
        shot artifex-20-icon-editor
        press 1041 448           # Back: the pen shape behind the line
        waitstate "$IC, Pen, 4, \"repl\"" 20 && ok "the shape list reorders" || bad "reorder: $(state)"
        press 772 630            # Save
        waitstate 'note=Some("Saved assets/icons/icon.svg")' 30 && ok "Save writes it" || bad "save: $(state)"
        check "the file is the editor's" "1" "$(V "grep -c 'artifex-icon v1' $W/proj/work/$P/assets/icons/icon.svg")"
        waitstate 'assets=Some((4, "assets/icons/icon.svg", 4, Some(0), Some(true)' 300 \
            && ok "the saved icon is decoded at four sizes and the dock's check passes" || bad "saved icon: $(state)"
        shot artifex-21-icon-saved
    else
        echo "  (no cargo-atrium with icon in the Rust layer: the check and Import SVG skipped)"
    fi
else
    echo "  (no atrium.toml project: skipped)"
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
