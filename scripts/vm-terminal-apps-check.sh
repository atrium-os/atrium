#!/bin/sh
# The Terminal runs full-screen programs (dev-tools-plan.md M1).
#
# 1. On the host: stoa-term's unit tests and the replay of recorded
#    sessions (test-assets/terminal, made by scripts/terminal-record.sh:
#    vi, vim, Neovim, less, top, and Helix when recorded) against the
#    screens tmux showed.
# 2. In the amd64 cloud VM with frescod headless, through the whole path
#    (portcullisd, stoad, Stoa's grid diffs, the Terminal): TERM is
#    xterm-256color; vi opens on the alternate screen, edits, writes and
#    gives the shell its screen back; less pages; top draws and quits;
#    Neovim and Helix too when the Scratch workspace has them (after M2).
#    The Terminal's trace line shows `<alt>` on the alternate screen.
#
# On-screen checks that need a person's eyes or the clipboard (a frame of
# each editor, a click moving Helix's cursor, a bracketed paste) run on the
# Mac VM.
#
#   sh scripts/vm-terminal-apps-check.sh [out-dir]     (HOST_ONLY=1: part 1 only)
# Ends "terminal-apps: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/terminal-apps-shots}
mkdir -p "$OUT"
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }

echo "=== replay (host)"
R=$(cd "$ROOT/stoa" && cargo test -q -p stoa-term 2>&1)
echo "$R" | grep -q "test result: FAILED" && bad "stoa-term tests: $(echo "$R" | grep -A12 'differ\|panicked' | head -30)" \
    || ok "stoa-term unit tests and $(ls "$ROOT/test-assets/terminal"/*.out | wc -l | tr -d ' ') recorded sessions"
if [ "${HOST_ONLY:-}" = 1 ]; then echo "terminal-apps: $fails failed"; [ $fails = 0 ]; exit; fi

export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "terminal-apps: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe"
xb atrium-term-socket "--bin atrium-term-socket --example console_probe"
xb stoa "-p stoa --bin stoad"

echo "=== install"
B=/var/tmp/termapps
V "pkill -x portcullisd; pkill -x frescod; pkill -x stoad; pkill -f atrium-term-socket; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
# The system faces frescod resolves (system-mono is IBM Plex Mono).
V "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    V "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || scp -q $K -P 2222 "$f" root@localhost:/usr/local/share/fonts/ibm-plex/
done
# A fresh VM has no alice (the open-files gates' user, uid 1011).
V "id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe \
         atrium-term-socket/target/$TARGET/release/atrium-term-socket atrium-term-socket/target/$TARGET/release/examples/console_probe \
         stoa/target/$TARGET/release/stoad; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "terminal-apps: 1 failed"; exit 1; }
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
STAY=org.test.stay; SYS=org.test.sys; LAUNCHER=org.test.launcher; TERM_ID=org.atrium.terminal; WS_ID=org.atrium.workspaces
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
mk() { # id name binary-in-B entry-name extra-capabilities
  opifex uninstall \$1 >/dev/null 2>&1 || true
  rm -rf /home/alice/.atrium/apps/\$1
  O=$B/b-\$1; rm -rf \$O; mkdir -p \$O/bin; cp $B/\$3 \$O/bin/\$4
  printf '[app]\nid = "%s"\nname = "%s"\nversion = "1"\nentry = "bin/%s"\n' \$1 "\$2" \$4 > \$O/atrium.toml
  [ -n "\$5" ] && printf '\n[capabilities]\n%b\n' "\$5" >> \$O/atrium.toml
  [ -n "\${6:-}" ] && printf '%b' "\$6" > \$O/\$7
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
}
mk $TERM_ID Terminal atrium-term-socket atrium-term-socket 'graphics = "fresco"\nconsoles = true' 'on\n' .trace
mkdir -p /atrium/sockets/portcullis /var/run/atrium; rm -f $PSOCK /var/run/atrium/stoad.sock
: > $LOG
daemon -f -o $B/stoad.log env STOA_CTL=/var/run/atrium/stoad.sock STOA_STATE=off $B/stoad
sleep 1; chmod 600 /var/run/atrium/stoad.sock
daemon -f -o $LOG env PORTCULLISD_FRESCO_ACTIVATION=$B/activation.sock /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd and the apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "terminal-apps: $fails failed"; exit 1; }
for a in $TERM_ID; do V "su -m alice -c 'portcullis policy grant $a' >/dev/null 2>&1; portcullis policy grant --system $a >/dev/null 2>&1"; done

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
# Tier-2 rendering needs AVX2: on the default Nehalem TCG CPU frescod dies
# of SIGILL at its first frame (boot with QEMU_BIN=<fork> TCG_CPU=Haswell-noTSX).
V "grep -q AVX2 /var/run/dmesg.boot" || { bad "the VM's CPU has no AVX2: cloud-vm.sh down, then up with QEMU_BIN=\$HOME/qemu-atrium/build/qemu-system-x86_64 TCG_CPU=Haswell-noTSX"; echo "terminal-apps: $fails failed"; exit 1; }
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "terminal-apps: $fails failed"; exit 1; }
inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $B/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-4}; }
shot() { sleep 3; get $B/screen.png "$OUT/$1.png" 2>/dev/null || get $B/screen "$OUT/$1.png" 2>/dev/null; [ -s "$OUT/$1.png" ] && echo "  screenshot $1.png" || echo "  (no screenshot for $1)"; }
# Type text into the focused window (the terminal tracks Shift by key).
typ() {
    python3 -I - "$1" > /tmp/workspace-keys.txt <<'PY'
import sys
lo = {c: 0x04 + i for i, c in enumerate("abcdefghijklmnopqrstuvwxyz")}
lo.update({c: 0x1e + i for i, c in enumerate("1234567890")})
lo.update({" ": 0x2c, "-": 0x2d, "=": 0x2e, "/": 0x38, ".": 0x37, ";": 0x33, "'": 0x34, ",": 0x36, "[": 0x2f, "]": 0x30, "\n": 0x28})
hi = {"{": 0x2f, "}": 0x30, "$": 0x21, "+": 0x2e, "!": 0x1e, "~": 0x35, ">": 0x37, "<": 0x36, "_": 0x2d, "|": 0x31, ":": 0x33, '"': 0x34, "&": 0x24, "(": 0x26, ")": 0x27, "*": 0x25}
hi.update({c.upper(): 0x04 + i for i, c in enumerate("abcdefghijklmnopqrstuvwxyz")})
for c in sys.argv[1]:
    if c in lo:
        print(f"KEY {lo[c]:#x} 1"); print(f"KEY {lo[c]:#x} 0")
    elif c in hi:
        print("KEY 0xe1 1 2"); print(f"KEY {hi[c]:#x} 1 2"); print(f"KEY {hi[c]:#x} 0 2"); print("KEY 0xe1 0 0")
PY
    scp -q $K -P 2222 /tmp/workspace-keys.txt root@localhost:$B/keys.txt
    V "nc -N -U $B/inject.sock < $B/keys.txt" >/dev/null
    sleep ${GAP:-4}
}
screen() { V "grep 'atrium-term: screen' $LOG | tail -1"; }
waitscreen() { # pattern [secs]
    i=0; while [ $i -lt ${2:-30} ] && ! screen | grep -q -- "$1"; do sleep 2; i=$((i+2)); done
    screen | grep -q -- "$1"
}

echo "=== jaild"
V "service atrium-jaild status >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1; sleep 1; test -S /var/run/atrium/jaild.sock" && ok "jaild is up" || bad "no jaild"
screen() { V "grep 'atrium-term: screen' $B/term.log | tail -1"; }
V "pkill -f atrium-term-socket; sleep 2; for j in \$(jls name | grep '^org_atrium_terminal\$'); do jail -r \$j; done"
V "for j in \$(jls name | grep '^empty_alice\$'); do jail -r \$j; done; for m in \$(mount -p | awk '\$2 ~ /atrium\/empty\/alice/ {print \$2}' | sort -r); do umount -f \$m; done"
V "su -m alice -c 'portcullis workspace list'" >/dev/null 2>&1   # makes Scratch
k1() { inj "KEY $1 1" "KEY $1 0"; sleep ${GAP:-4}; }
esc() { k1 0x29; }
chord() { # a key with Ctrl+Shift
    inj "KEY 0xe0 1 1" "KEY 0xe1 1 3" "KEY $1 1 3" "KEY $1 0 3" "KEY 0xe1 0 1" "KEY 0xe0 0 0"; sleep ${GAP:-4}
}
gone_alt() { i=0; while [ $i -lt 30 ] && screen | grep -q "<alt>"; do sleep 2; i=$((i+2)); done; ! screen | grep -q "<alt>"; }

echo "=== the empty console"
V "daemon -f -o $B/term.log su -m alice -c 'portcullis launch --no-prompt $TERM_ID'"
waitscreen "empty \\$" 90 && ok "Terminal opens the empty console" || bad "no prompt: $(screen) $(V "tail -5 $LOG")"
typ "echo term=\$TERM
"
waitscreen "term=xterm-256color" 20 && ok "TERM is xterm-256color" || bad "TERM: $(screen)"

echo "=== vi"
typ "vi /tmp/gate.txt
"
waitscreen "<alt>" 30 && ok "vi takes the alternate screen" || bad "vi: $(screen)"
waitscreen "| ~ | ~ |" 10 && ok "with its empty-line tildes" || bad "tildes: $(screen)"
# nvi flushes the input queue as it sets up the terminal (TCSAFLUSH), which
# under TCG ends well after its first frame: let it settle before typing.
sleep 5
typ "ihello from vi"
esc
waitscreen "hello from vi" 10 && ok "insert shows the text" || bad "insert: $(screen)"
typ "0lllll"
S=$(screen); echo "$S" | grep -q "@0,5 " && ok "the cursor follows motions" || bad "cursor: $S"
typ ":wq
"
gone_alt && ok "leaving vi gives the shell its screen back" || bad "still alternate: $(screen)"
waitscreen "echo term=" 10 && ok "the shell's lines are there again" || bad "main screen: $(screen)"
shot apps-1-vi
typ "cat /tmp/gate.txt
"
waitscreen "| hello from vi | empty" 20 && ok "vi wrote the file" || bad "write: $(screen)"

echo "=== less"
typ "i=0; while [ \$i -lt 120 ]; do echo row \$i; i=\$((i+1)); done > /tmp/rows; less /tmp/rows
"
waitscreen "<alt>" 30 && ok "less takes the alternate screen" || bad "less: $(screen)"
waitscreen "| row 0 |" 10 && ok "showing the top" || bad "top rows: $(screen)"
k1 0x2c
waitscreen "| row 4[0-9] |" 10 && ok "Space pages down" || bad "page: $(screen)"
typ "q"
gone_alt && ok "q leaves it" || bad "less stayed: $(screen)"

echo "=== Scratch: top, Neovim, Helix"
chord 0x12
k1 0x51
k1 0x28
waitscreen "scratch \\$" 60 && ok "in the Scratch workspace" || bad "switch: $(screen)"
typ "top
"
# FreeBSD's top draws on the main screen (it sends no ti/te).
waitscreen "last pid.*PID USERNAME" 40 && ok "top draws its table" || bad "top: $(screen)"
shot apps-2-top
typ "q"
i=0; while [ $i -lt 20 ] && ! screen | sed 's/.*| //' | grep -q 'scratch \$'; do sleep 2; i=$((i+2)); done
screen | sed 's/.*| //' | grep -q 'scratch \$' && ok "q leaves top for the prompt" || bad "top stayed: $(screen)"
for ed in nvim hx; do
    if V "jexec ws_alice_scratch sh -c 'command -v $ed'" >/dev/null 2>&1; then
        typ "$ed /tmp/gate.txt
"
        waitscreen "<alt>.*hello from vi" 60 && ok "$ed opens the file" || bad "$ed: $(screen)"
        shot apps-3-$ed
        esc
        typ ":q
"
        gone_alt && ok "$ed quits" || bad "$ed stayed: $(screen)"
    else
        echo "  skip $ed: not in Scratch (pkg in workspaces is M2)"
    fi
done

echo "terminal-apps: $fails failed"
[ $fails = 0 ]
