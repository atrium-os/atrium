#!/bin/sh
# The dock's Applications view, JAILED (docs/spec/forum.md §6.1), in the
# amd64 cloud VM: frescod and forum-wm at their /atrium/sockets paths;
# portcullisd launches forum-dock from its signed bundle (graphics,
# forum-control, app-launch), and the dock launches two signed sample apps
# through portcullisd, each in a jail of its own. Runs on the HOST; ends
# "apps-grid-jail: N failed".
#
#   TARGETS=x86_64-softmmu sh scripts/build-qemu.sh       (once)
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 \
#     TCG_CPU=Haswell-noTSX sh scripts/cloud-vm.sh up
#   sh scripts/vm-apps-grid-jail-check.sh [out-dir]
#
# (frescod's Tier-2 renderer needs AVX2: hence Haswell-noTSX, which needs the
# fork QEMU.) Builds what it needs (portcullis workspace, frescod, forum-wm,
# forum-dock and its sample_app example).
#
# Checks: the jailed dock gets the catalog from portcullisd (not the app
# tree); Super+A opens the view; typing "beta" and Enter launches Beta in its
# own jail as its own uid; the view closes; the dock shows Beta running (its
# uid from the catalog matches its window's owner); pressing Beta's dock
# tile again is AlreadyRunning and focuses it. Then the dock as a SESSION
# COMPONENT (portcullisd's LaunchSessionComponent, as ostiarius asks at
# login): it gets the person's state folder, read-write at /atrium/state;
# Ctrl+D unpins the selected app, the pins land in
# ~/.atrium/state/org.atrium.forum-dock/pinned (homes.md §2; root's home
# here), and they are
# read back after a teardown and a fresh launch. Alpha ships its own SVG icon,
# which portcullisd reads from the bundle and the dock draws as filled paths.
# The person has a Work persona (personas.md §7): launching from the grid asks
# which (Enter: Personal); Beta opened in Work gets a dock tile of its own
# beside Personal's, with a blue badge, and a press on it raises Work's.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-/tmp/apps-grid-jail}
export ARCH=amd64 TARGET=x86_64-unknown-freebsd
T=$TARGET
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }
mkdir -p "$OUT"
W=/root/agj
S=/tmp/agj
PSOCK=/atrium/sockets/portcullis/portcullis.sock
fails=0
ok() { echo "  ok: $*"; }
bad() { echo "  FAIL: $*"; fails=$((fails + 1)); }
die() { echo "  FATAL: $*"; echo "apps-grid-jail: 1 failed"; exit 1; }

g true || die "the VM does not answer over ssh"
g "grep -q AVX2 /var/run/dmesg.boot" || die "the guest has no AVX2 (boot with TCG_CPU=Haswell-noTSX and the fork QEMU)"

echo "== cross-build"
RF=$(cd "$ROOT" && sh -c '. ./scripts/cloud-vm.sh >/dev/null 2>&1; printf %s "$CROSS_RUSTFLAGS"' sh env)
[ -n "$RF" ] || die "no CROSS_RUSTFLAGS (cloud-vm.sh setup first)"
(cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$RF" \
    cargo build --quiet --release --target $T) || die "portcullis cross-build"
for c in frescod forum-wm forum-dock; do
  (cd "$ROOT/$c" && RUSTFLAGS="$RF" cargo build --quiet --release --target $T --bins) || die "$c cross-build"
done
(cd "$ROOT/forum-dock" && RUSTFLAGS="$RF" cargo build --quiet --release --target $T --example sample_app) || die "sample_app"

echo "== deploy the portcullis stack"
sh "$ROOT/scripts/deploy-portcullis.sh" 2>&1 | grep -v -e '^warning' -e '^ *|' -e '^ \+= ' | tail -2
PT="$ROOT/portcullis/target/$T/release"
for b in portcullisd portcullis opifex; do
  h=$(sha256sum "$PT/$b" | cut -d' ' -f1); gh=$(g "sha256 -q /usr/local/bin/$b" | tr -d '\r')
  # deploy-portcullis.sh builds with -p (other features): use the workspace build.
  if [ "$h" != "$gh" ]; then
    put "$PT/$b" /tmp/$b.new && g "install -m 755 /tmp/$b.new /usr/local/bin/$b && rm /tmp/$b.new" || die "install $b"
    gh=$(g "sha256 -q /usr/local/bin/$b" | tr -d '\r')
    [ "$h" = "$gh" ] || die "/usr/local/bin/$b is not the build just made"
  fi
done

echo "== copy binaries"
g "pkill -x frescod; pkill -x forum-wm; pkill -x forum-dock; pkill -x beta; pkill -x alpha; sleep 1; rm -rf $W $S; mkdir -p $W $S; chmod 777 $S"
for b in frescod/target/$T/release/frescod frescod/target/$T/release/fresco-renderer \
         forum-wm/target/$T/release/forum-wm forum-dock/target/$T/release/forum-dock \
         forum-dock/target/$T/release/examples/sample_app portcullis/target/$T/release/sesstest; do
  put "$ROOT/$b" "$W/" || die "copy $b"
  h=$(sha256sum "$ROOT/$b" | cut -d' ' -f1); gh=$(g "sha256 -q $W/$(basename "$b")" | tr -d '\r')
  [ "$h" = "$gh" ] || die "$(basename "$b") hash mismatch"
done
put "$ROOT/forum-dock/atrium.toml" "$W/dock.toml"
# Alpha ships its own icon (forum.md §6.1): an orange tile with a white "A".
cat > "$OUT/alpha.svg" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48" width="48" height="48">
  <rect width="48" height="48" rx="11" fill="#f4511e"/>
  <path d="M24 9 L37 39 H31 L28 31 H20 L17 39 H11 Z M22 26 H26 L24 20 Z" fill="#ffffff" fill-rule="evenodd"/>
</svg>
SVG
put "$OUT/alpha.svg" "$W/alpha.svg"
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
  g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "== install signed bundles"
g "set -e
  test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
  openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
  mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
  for id in org.atrium.forum-dock org.example.alpha org.example.beta; do /usr/local/bin/opifex uninstall \$id >/dev/null 2>&1 || true; done
  B=$W/b-dock; mkdir -p \$B/bin; cp $W/forum-dock \$B/bin/; cp $W/dock.toml \$B/atrium.toml
  openssl dgst -sha256 -sign /root/e2e-key.pem -out \$B/atrium.toml.sig \$B/atrium.toml
  /usr/local/bin/opifex install \$B
  for n in alpha:Alpha:icons/alpha.svg beta:Beta:note-pencil; do
    id=\${n%%:*}; rest=\${n#*:}; name=\${rest%%:*}; icon=\${rest#*:}
    B=$W/b-\$id; mkdir -p \$B/bin; cp $W/sample_app \$B/bin/\$id
    [ \$id = alpha ] && mkdir -p \$B/icons && cp $W/alpha.svg \$B/icons/
    printf '[app]\nid = \"org.example.%s\"\nname = \"%s\"\nversion = \"1.0.0\"\nentry = \"bin/%s\"\nicon = \"%s\"\n\n[capabilities]\ngraphics = \"fresco\"\nnetwork = \"none\"\n\n[supervision]\ninstances = \"single\"\n' \$id \$name \$id \$icon > \$B/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out \$B/atrium.toml.sig \$B/atrium.toml
    /usr/local/bin/opifex install \$B
  done
  for id in org.atrium.forum-dock org.example.alpha org.example.beta; do /usr/local/bin/portcullis policy grant \$id; done
  P=/var/db/atrium/root/policy.toml; rm -f /var/db/atrium/root/persona-last; sed -i '' '/^\[personas.work\]/,+1d' \$P; printf '[personas.work]\ncolour = \"blue\"\n' >> \$P" \
  > "$OUT/install.log" 2>&1 || die "install: $(tail -5 "$OUT/install.log")"
tail -3 "$OUT/install.log"

echo "== frescod, forum-wm, portcullisd at their jail paths"
g "mkdir -p /atrium/sockets/fresco /atrium/sockets/fresco-wm /atrium/sockets/forum-ctl /atrium/sockets/portcullis
  rm -f /atrium/sockets/fresco/fresco.sock /atrium/sockets/fresco-wm/fresco-wm.sock /atrium/sockets/forum-ctl/forum-ctl.sock
  pkill -x portcullisd; sleep 1; rm -f $PSOCK
  daemon -f -o $S/portcullisd.log /usr/local/bin/portcullisd --socket $PSOCK; sleep 3; test -S $PSOCK" || die "portcullisd start"
g "cd $S && env RUST_LOG=${RUST_LOG:-warn} FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$W/fresco-renderer \
    FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock \
    FRESCOD_WM_SOCK=/atrium/sockets/fresco-wm/fresco-wm.sock FRESCOD_INPUT_SOCK=$S/inject.sock daemon -f -o $S/frescod.log $W/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
sleep 20
g "pgrep -x frescod >/dev/null" || die "frescod is not running: $(g "tail -3 $S/frescod.log")"
g "chmod 666 /atrium/sockets/fresco/fresco.sock; cd $S && env FRESCO_WM_SOCKET=/atrium/sockets/fresco-wm/fresco-wm.sock FORUM_CTL_SOCKET=/atrium/sockets/forum-ctl/forum-ctl.sock daemon -f -o $S/wm.log $W/forum-wm </dev/null"
sleep 8

echo "== portcullisd launches the dock, jailed"
g "cd $S && daemon -f -o $S/dock.log /usr/local/bin/portcullis launch --no-prompt org.atrium.forum-dock </dev/null"
sleep ${SETTLE:-60}
inj() { g "printf '$1\\n' | nc -N -U $S/inject.sock"; sleep ${KGAP:-2}; }
key() { inj "KEY $1 1 ${2:-0}"; inj "KEY $1 0 ${2:-0}"; }
press() { inj "MOVE $1 $2"; inj "BTN 1 1"; inj "BTN 1 0"; }
shot() { sleep ${GAP:-20}; g "cp $S/screen.png $S/shot.png"; get $S/shot.png "$OUT/$1.png"; }
has() { g "grep -q -- \"$1\" $S/dock.log"; }

g "jls name | grep -q forum-dock" && ok "the dock runs in a jail" || bad "no forum-dock jail"
has "app(s) from Daemon" && ok "catalog from portcullisd" || bad "the dock did not get the catalog from portcullisd"
g "grep -q 'catalog -> org.atrium.forum-dock' $S/portcullisd.log" && ok "portcullisd served the dock's catalog" || bad "portcullisd log has no catalog for the dock"
has "from Daemon, 1 with their own icon" && ok "Alpha's bundle icon came through the catalog and parsed" || bad "no bundle icon: $(g "grep -E 'icon is not drawn|from Daemon' $S/dock.log | tail -2")"
shot 0-dock

echo "== Super+A, type beta, Enter"
key 0x04 0x08
sleep 10
has "Applications view open" && ok "Super+A opened the view in the jailed dock" || bad "Super+A did not open it"
for k in 0x05 0x08 0x17 0x04; do key $k; done   # b e t a
sleep 6
g "grep 'apps: ' $S/dock.log | tail -1" | grep -q 'selected=org.example.beta' && ok "\"beta\" selects Beta" || bad "Beta not selected"
shot 1-filtered
key 0x28
sleep 20
has "persona question for Beta" && ok "Beta: which persona? asked" || bad "no persona question: $(g "grep -i persona $S/dock.log | tail -2")"
key 0x28
sleep 30
has "launched 'org.example.beta'" && ok "portcullisd accepted the launch" || bad "no launch: $(g "grep -E 'beta|launch' $S/dock.log | tail -2")"
g "jls name | grep -q example.beta" && ok "Beta runs in its own jail" || bad "no Beta jail"
has "Applications view closed" && ok "the view closed on the launch" || bad "the view stayed up"
sleep 10
shot 2-beta-up

echo "== the dock shows Beta running"
sleep 10
g "grep 'running:' $S/dock.log | tail -1" | tee "$OUT/running.txt"
grep -q 'org.example.beta' "$OUT/running.txt" && ok "Beta's window matched to Beta by uid" || bad "the dock does not show Beta running"
inj "MOVE 10 300"
shot 3-running

echo "== Beta's dock tile again: AlreadyRunning, focused"
BTN=$(g "grep 'Applications button at' $S/dock.log | tail -1" | sed 's/.* at //')
bx=${BTN%,*}; by=${BTN#*,}
# Default pins: the first five listed (Alpha, Beta): Beta is the third tile.
press $((bx + 152)) "$by"
sleep 20
key 0x28   # the persona question again: Personal, last used
sleep 30
has "'org.example.beta' is already running" && ok "a second press is AlreadyRunning" || bad "no AlreadyRunning: $(g "tail -3 $S/dock.log")"
has "asked the WM to focus its surface" && ok "the dock asked forum-wm to focus Beta" || bad "Beta's window not found by its uid"

echo "== Beta in Work: a tile of its own, badged"
g "cd $S && daemon -f -o $S/beta-work.log /usr/local/bin/portcullis launch --no-prompt --persona work org.example.beta </dev/null"
sleep 45
g "grep 'running:' $S/dock.log | tail -1" | tee "$OUT/running-work.txt"
grep -q 'org.example.beta@work' "$OUT/running-work.txt" && grep -q 'org.example.beta,\|org.example.beta]' "$OUT/running-work.txt" \
  && ok "the dock tells Work's Beta from Personal's by uid" || bad "Work's Beta not shown running: $(g "tail -3 $S/beta-work.log")"
inj "MOVE 10 300"
shot 6-work-tile
BTN=$(g "grep 'Applications button at' $S/dock.log | tail -1" | sed 's/.* at //')
bx=${BTN%,*}; by=${BTN#*,}
# Tiles 76 pt apart: Alpha, Beta, then Beta in Work; its badge sits on the
# top-right corner (a 20 pt blue dot centred 24 right of and 24 above the
# tile's centre).
blue() { python3 -c "
from PIL import Image
im = Image.open('$OUT/6-work-tile.png').convert('RGB')
n = 0
for x in range($1 + 12, $1 + 37):
    for y in range($2 - 37, $2 - 12):
        r, g, b = im.getpixel((x, y))
        n += abs(r - 0x25) < 12 and abs(g - 0x63) < 12 and abs(b - 0xeb) < 12
print(n)"; }
# (The logged y is in the dock window, which starts under frescod's 24 pt strip.)
WB=$(blue $((bx + 228)) $((by + 24))); PB=$(blue $((bx + 152)) $((by + 24)))
echo "  badge pixels: Work's tile $WB, Personal's $PB"
[ "${WB:-0}" -gt 100 ] && ok "Work's tile has a blue badge" || bad "no blue badge on Work's tile ($WB)"
[ "${PB:-1}" = 0 ] && ok "Personal's tile has none" || bad "Personal's tile badged ($PB)"
NQ=$(g "grep -c 'persona question' $S/dock.log")
press $((bx + 228)) "$by"
sleep 30
has "'org.example.beta' is already running" && [ "$(g "grep -c 'asked the WM to focus its surface' $S/dock.log")" -ge 2 ] \
  && ok "a press on Work's tile raises Work's Beta, nothing asked" || bad "Work's tile: $(g "tail -3 $S/dock.log")"
g "grep -c 'persona question' $S/dock.log" | grep -qx "$NQ" && ok "and asked nothing" || bad "the tile asked which persona"
g "pkill -x beta; P=/var/db/atrium/root/policy.toml; sed -i '' '/^\[personas.work\]/,+1d' \$P"

echo "== the dock as a session component: pins kept in the person's state folder"
SD=/root/.atrium/state/org.atrium.forum-dock
JN=app-org-atrium-forum-dock
# The app-launched dock goes (portcullisd cleans its jail up); Beta too.
g "pkill -x forum-dock; pkill -x beta; sleep 5; rm -rf /var/db/atrium/state/root /root/.atrium/state
  mkdir -p /etc/atrium/session.d /usr/local/share/atrium/apps/org.atrium.forum-dock/bin
  install -m 755 $W/forum-dock /usr/local/share/atrium/apps/org.atrium.forum-dock/bin/forum-dock
  printf 'enabled = false\nname = \"gate-session-launcher\"\npath = \"/var/lib/atrium/jails/gate-session-launcher\"\n[capabilities]\nsession_launch = true\n[exec]\npath = \"/usr/bin/true\"\nargv = [\"true\"]\nuid = 0\ngid = 0\n' > /etc/atrium/services.d/99-gate-session-launcher.toml"
put "$ROOT/portcullis/ostiarius/etc/session.d/org.atrium.forum-dock.toml" /etc/atrium/session.d/
sess() { g "$W/sesstest $PSOCK $*"; }
sess launch $JN root | tee "$OUT/session-launch.txt"
grep -q SessionComponentLaunched "$OUT/session-launch.txt" && ok "launched as a session component" || bad "no session launch"
sleep ${SETTLE:-60}
g "stat -f '%Su %Lp' $SD" | tee "$OUT/state-dir.txt"
DU=$(g "ps -o uid= -p \$(pgrep -x forum-dock | head -1)" | tr -d ' \r')
grep -q " 700" "$OUT/state-dir.txt" && ok "state folder made, mode 700" || bad "no state folder"
[ -n "$DU" ] && g "test \$(stat -f %u $SD) = $DU" && ok "owned by the dock's session uid ($DU)" || bad "state folder not owned by the dock (uid ${DU:-?})"
g "mount -p | grep -q '^$SD[[:space:]].*/atrium/state[[:space:]]*nullfs[[:space:]]*rw'" \
  && ok "mounted into the dock's jail" || bad "state folder not mounted"
key 0x04 0x08
sleep 10
inj "KEY 0x07 1 0x01"; inj "KEY 0x07 0 0x01"   # Ctrl+D on the selected app (Alpha)
sleep 6
g "cat $SD/pinned 2>&1" | tee "$OUT/pinned.txt"
grep -q '^org.example.beta$' "$OUT/pinned.txt" && ! grep -q alpha "$OUT/pinned.txt" \
  && ok "Ctrl+D unpinned Alpha; the pins are in the state folder" || bad "pins not saved to the state folder"
key 0x29
shot 4-unpinned
sess teardown $JN | tee "$OUT/session-teardown.txt"
sleep 5
g "pgrep -x forum-dock" >/dev/null && bad "the dock outlived its teardown" || ok "torn down"
sess launch $JN root | grep -q SessionComponentLaunched && ok "launched again" || bad "second session launch"
sleep ${SETTLE:-60}
g "cat $SD/pinned" | grep -q '^org.example.beta$' && ok "pins survived the teardown" || bad "pins lost"
shot 5-relaunched
sess teardown $JN >/dev/null
g "rm -f /etc/atrium/services.d/99-gate-session-launcher.toml"

echo "== logs"
g "for f in portcullisd wm dock; do echo -- \$f; grep -v '^\$' $S/\$f.log | tail -10; done"
echo "apps-grid-jail: $fails failed"
[ "$fails" = 0 ]
