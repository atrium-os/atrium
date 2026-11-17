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
# CDEV=<c-dev layer tree> installs it first (build-c-dev-layer.sh's output).
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
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex"
xb atrium-term-socket "--bin atrium-term-socket"
xb stoa "-p stoa --bin stoad"
xb artifex "-p artifex --bin artifex"

echo "=== install"
B=/var/tmp/artifexchk
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; pkill -x artifex; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
V "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    V "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || scp -q $K -P 2222 "$f" root@localhost:/usr/local/share/fonts/ibm-plex/
done
V "id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex atrium-term-socket/target/$TARGET/release/atrium-term-socket \
         stoa/target/$TARGET/release/stoad artifex/target/$TARGET/release/artifex; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "artifex: 1 failed"; exit 1; }
done
scp -q $K -P 2222 "$ROOT/artifex/app/atrium.toml" root@localhost:$B/artifex.toml
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
TERM_ID=org.atrium.terminal; ART=org.atrium.artifex
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
sign() { openssl dgst -sha256 -sign /root/of-key.pem -out \$1/atrium.toml.sig \$1/atrium.toml; }
for id in $TERM_ID $ART; do opifex uninstall \$id >/dev/null 2>&1 || true; rm -rf /home/alice/.atrium/apps/\$id; done
O=$B/b-$TERM_ID; mkdir -p \$O/bin; cp $B/atrium-term-socket \$O/bin/
printf '[app]\nid = "$TERM_ID"\nname = "Terminal"\nversion = "1"\nentry = "bin/atrium-term-socket"\n\n[capabilities]\ngraphics = "fresco"\nconsoles = true\n' > \$O/atrium.toml
sign \$O; opifex install \$O >/dev/null
# Artifex with its own manifest, plus the check's two files: trace on, and
# the window at (40, 30).
O=$B/b-$ART; mkdir -p \$O/bin; cp $B/artifex \$O/bin/; cp $B/artifex.toml \$O/atrium.toml
echo on > \$O/.trace; echo 40,30 > \$O/.at
sign \$O; opifex install \$O >/dev/null
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

# The c-dev layer (clangd), when CDEV names its tree: signed with a gate
# key standing in for the platform key, as vm-layers-check.sh does. A C
# workspace made after it gets it on PATH.
CDEV=${CDEV:-}
if [ -n "$CDEV" ] && [ -f "$CDEV/atrium.toml" ]; then
    echo "=== the c-dev layer"
    (cd "$ROOT/portcullis" && cargo build -q -p opifex) || { echo "host opifex failed"; exit 1; }
    LH=$(mktemp -d)
    openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out "$LH/gate.key"
    openssl ec -in "$LH/gate.key" -pubout -out "$LH/gate.pem" 2>/dev/null
    # The manifest without its content line (the [layer.env] after it stays).
    grep -v '^content = ' "$CDEV/atrium.toml" > "$LH/c-dev.toml"
    "$ROOT/portcullis/target/debug/opifex" layer pack "$CDEV" --manifest "$LH/c-dev.toml" --sign "$LH/gate.key" >/dev/null || bad "pack c-dev"
    (cd "$(dirname "$CDEV")" && tar -czf "$LH/c-dev.tar.gz" "$(basename "$CDEV")")
    want=$(sha256sum "$LH/c-dev.tar.gz" | cut -d' ' -f1)
    scp -q $K -P 2222 "$LH/c-dev.tar.gz" "$LH/gate.pem" root@localhost:$B/
    [ "$want" = "$(V "sha256 -q $B/c-dev.tar.gz")" ] || bad "c-dev arrived damaged (TCG scp); run again"
    out=$(V "cd $B && tar -xf c-dev.tar.gz && rm c-dev.tar.gz && mkdir -p /etc/atrium/platform-keys && cp gate.pem /etc/atrium/platform-keys/artifex-gate.pem && \
       for w in scratch proj; do su -m alice -c \"portcullis workspace stop \$w\" >/dev/null 2>&1; done; \
       for j in \$(jls name | grep '^ws_alice_'); do jail -r \$j; done; \
       for m in \$(mount -p | awk '\$2 ~ /toolchains/ {print \$2}' | sort -r); do umount -f \$m; done; \
       (opifex layer remove c-dev@1 >/dev/null 2>&1; opifex layer install $B/$(basename "$CDEV"))" 2>&1)
    V "opifex layer list" | grep -q " c-dev@1 " && V "grep -q LD_LIBRARY_PATH /var/lib/atrium/layers/c-dev@1/atrium.toml" \
        && ok "c-dev installed" || bad "c-dev: $out"
    rm -rf "$LH"
fi

echo "=== the workspace"
V "service atrium-jaild status >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1; sleep 1; test -S /var/run/atrium/jaild.sock" && ok "jaild is up" || bad "no jaild"
W=/home/alice/.atrium/workspaces
for w in scratch proj; do V "su -m alice -c 'portcullis workspace stop $w'" >/dev/null 2>&1; done
V "for m in \$(mount -p | awk '\$2 ~ /workspaces\/alice|jails\/$ART\/work/ {print \$2}' | sort -r); do umount -f \$m; done; rm -rf $W /var/lib/atrium/workspaces/alice /var/db/atrium/alice/workspace-picks /var/run/atrium/ide"
V "su -m alice -c 'portcullis workspace list'" >/dev/null 2>&1
V "su -m alice -c 'portcullis workspace new proj --network none'" >/dev/null 2>&1
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

echo "=== terminal"
press 1080 52              # the bar's Terminal button
i=0; while [ $i -lt 30 ] && ! V "grep -q 'console in workspace/proj' $LOG"; do sleep 2; i=$((i+2)); done
V "grep -q 'console in workspace/proj' $LOG" && ok "Terminal opens proj's terminal" || bad "terminal: $(V "grep -i console $LOG | tail -2")"
shot artifex-4-terminal

echo "=== clangd"
if V "jexec ws_alice_proj test -x /atrium/toolchains/c-dev/bin/clangd"; then
    waitstate 'lsp=Some("clangd") diags=[2-9]' 120 && ok "clangd answers, with its own diagnostic" || bad "clangd: $(state)"
else
    echo "  (no clangd in proj: the c-dev layer isn't installed here; skipped)"
fi

echo "artifex: $fails failed"
[ $fails = 0 ]
