#!/bin/sh
# Share sheet gate, confined (docs/spec/share-sheet.md §6): the same share
# as scripts/vm-share-check.sh, with the real pieces. The sharing app and the
# targets are dev builds Run from alice's Scratch workspace, each in its own
# jail; the real limend (as _limen) asks portcullisd to start the chosen
# target for the app's instance; frescod serves the apps through the jailed
# socket and lets only _limen bind slots and only _share spend SHARE; the
# share sheet runs as _share. Runs on the HOST:
#
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 TCG_CPU=Haswell-noTSX \
#     sh scripts/cloud-vm.sh up
#   sh scripts/vm-share-jail-check.sh [out-dir]
#
# Ends "share-jail: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/share-jail}
WAIT=${WAIT:-90}
GAP=${GAP:-12}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { ssh $K -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp -q $K -P 2222 "$1" "root@localhost:$2"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
end() { echo "share-jail: $fails failed"; exit $fails; }
mkdir -p "$OUT"
V true >/dev/null 2>&1 || { echo "the VM does not answer over ssh"; fails=1; end; }

if [ "${BUILD:-1}" = 1 ]; then
    echo "== cross-build"
    xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; fails=1; end; }; }
    xb frescod "--bin frescod"
    xb portcullis "-p portcullisd -p portcullis-cli --bin portcullisd --bin portcullis"
    xb limen "--bin limend"
    xb share-sheet "--bins"
fi

B=/var/tmp/share-jail
R="$TARGET/release"
# A jail of the last run outlives its app when portcullisd is killed under
# it, and portcullisd would take it for a running app.
GONE='for j in $(jls name | grep "^dev_alice_scratch_"); do jail -r $j; done 2>/dev/null; true'
V "pkill -x frescod; pkill -x portcullisd; pkill -x limend; pkill -f share-sheet; sleep 1; $GONE; mkdir -p $B; chmod 755 $B; true"
for b in "$ROOT/frescod/target/$R/frescod" "$ROOT/portcullis/target/$R/portcullisd" "$ROOT/portcullis/target/$R/portcullis" \
         "$ROOT/limen/target/$R/limend" "$ROOT/share-sheet/target/$R/share-sheet" "$ROOT/share-sheet/target/$R/share-demo" \
         "$ROOT/share-sheet/target/$R/share-target-demo" "$ROOT/etc/jaild.policy.toml"; do
    h=$(sha256sum "$b" | cut -d' ' -f1)
    [ "$h" = "$(V "sha256 -q $B/$(basename "$b") 2>/dev/null || true")" ] && continue
    put "$b" "$B/"
    [ "$h" = "$(V "sha256 -q $B/$(basename "$b")")" ] || { echo "ABORT: $(basename "$b") arrived damaged"; fails=1; end; }
done
V "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    V "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "== frescod, portcullisd, limend, the share sheet"
PSOCK=/atrium/sockets/portcullis/portcullis.sock
FS=/atrium/sockets/fresco
ACT=/var/run/atrium/fresco-activation.sock
LOG=$B/portcullisd.log; LLOG=$B/limend.log; FLOG=$B/frescod.log; SLOG=$B/sheet.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/frescod /usr/local/bin/frescod
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/limend /usr/local/bin/limend
install -m 0755 $B/share-sheet /usr/local/bin/share-sheet
mkdir -p /etc/atrium
sed -e 's/^mode     = 1/mode     = 2/' $B/jaild.policy.toml > /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
id _limen >/dev/null 2>&1 || pw useradd _limen -u 65022 -d /nonexistent -s /usr/sbin/nologin
id _share >/dev/null 2>&1 || pw useradd _share -u 65045 -d /nonexistent -s /usr/sbin/nologin
service atrium-jaild restart >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
mkdir -p /atrium/sockets/portcullis /atrium/sockets/limen $FS /var/run/atrium
rm -f $PSOCK /atrium/sockets/limen/limend.sock $FS/fresco.sock $ACT
chown _limen /atrium/sockets/limen
: > $LOG; : > $LLOG; : > $FLOG; : > $SLOG; chmod 666 $LLOG $SLOG
cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=$FS/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
  FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_INJECT_ACTIVATION=1 FRESCOD_TOKEN_LIFE_MS=15000 \
  FRESCOD_ACTIVATION_SOCK=$ACT FRESCOD_ACTIVATION_UIDS=65045=share FRESCOD_SLOT_UIDS=65022 FRESCOD_DEV_CAPS=all \
  daemon -f -o $FLOG /usr/local/bin/frescod </dev/null
i=0; while [ \$i -lt 60 ] && ! test -S $FS/fresco.sock; do sleep 2; i=\$((i+1)); done
test -S $FS/fresco.sock
chmod 666 $FS/fresco.sock $ACT
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
daemon -f -u _limen -o $LLOG env LIMEN_FRESCOD_ACTIVATION=$ACT /usr/local/bin/limend
sleep 2; test -S /atrium/sockets/limen/limend.sock
chmod a+r /var/run/atrium/app-registry 2>/dev/null || true
daemon -f -u _share -o $SLOG env FRESCO_SOCKET=$FS/fresco.sock SHARE_FRESCOD_ACTIVATION=$ACT /usr/local/bin/share-sheet
EOS
[ $? = 0 ] || { bad "setup: $(V "tail -3 $FLOG $LOG $LLOG")"; end; }
sleep 5
V "pgrep -x frescod >/dev/null" && ok "frescod up (only _limen binds, only _share spends SHARE)" || { bad "frescod: $(V "tail -3 $FLOG")"; end; }
ok "portcullisd, limend as _limen"
i=0; while [ $i -lt 30 ] && ! V "grep -q 'providing share' $SLOG"; do sleep 1; i=$((i+1)); done
V "grep -q 'providing share' $SLOG" && ok "the share sheet provides share, as _share (limend took its uid)" || bad "sheet: $(V "tail -3 $SLOG $LLOG | tr '\n' ';'")"

echo "== the apps, Run from Scratch"
A() { V "su -m alice -c 'cd /tmp; portcullis workspace $1'" 2>&1; }
W=/home/alice/.atrium/workspaces
A "stop scratch" >/dev/null
A "new scratch --network none" >/dev/null 2>&1
mk() { # folder id name bin extra-toml
    V "rm -rf $W/scratch/work/$1; mkdir -p $W/scratch/work/$1/bin; install -m 0755 $B/$4 $W/scratch/work/$1/bin/app; \
       printf '[app]\nid = \"$2\"\nname = \"$3\"\nversion = \"1\"\nentry = \"bin/app\"\n$5' > $W/scratch/work/$1/atrium.toml; \
       chown -R alice:files-alice $W/scratch/work/$1"
}
mk notes org.test.notes Notes share-demo '\n[capabilities]\ngraphics = \"fresco\"\nembeds = [\"share\"]\n'
V "printf '\211PNG\r\n\032\n-a-picture-' > $W/scratch/work/notes/share-me.png; chown alice $W/scratch/work/notes/share-me.png"
mk scraps org.test.scraps Scraps share-target-demo '\n[roles]\nimplements = [\"share-target\"]\n\n[roles.share-target]\nmime = [\"text/*\", \"image/*\"]\n\n[resources]\nmemory = \"256M\"\n'
mk photos org.test.photos Photos share-target-demo '\n[roles]\nimplements = [\"share-target\"]\n\n[roles.share-target]\nmime = [\"image/*\"]\n\n[resources]\nmemory = \"256M\"\n'
# Targets are installed by a Run (and end at once: no limend channel).
for t in scraps photos; do
    out=$(A "run scratch $t"); case "$out" in dev.alice.scratch.org.test.$t*) ok "$t installed" ;; *) bad "$t: $out" ;; esac
done
V "sleep 3; pkill -f 'dev.alice.scratch.org.test.scraps|dev.alice.scratch.org.test.photos' >/dev/null 2>&1; true"
out=$(A "run scratch notes"); case "$out" in dev.alice.scratch.org.test.notes*) ok "Notes runs in its jail" ;; *) bad "notes: $out" ;; esac
NU=$(V "awk '\$3 == \"dev.alice.scratch.org.test.notes\" {print \$1}' /var/run/atrium/app-registry | tail -1")
sleep $WAIT

inj() { V "printf '%s\n' '$1' | nc -N -U $B/inject.sock"; }
press() { inj "MOVE $1 $2"; sleep 1; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep $GAP; }
shot() { sleep ${1:-6}; get $B/screen.png "$OUT/$2.png" 2>/dev/null || get "$B/screen" "$OUT/$2.png" 2>/dev/null; [ -s "$OUT/$2.png" ] && echo "  screenshot $2.png" || echo "  (no screenshot for $2)"; }
logs="$LOG $LLOG $SLOG"
has() { V "cat $logs | grep -q -- '$1'"; }

echo "== share a note and a picture"
shot 2 jail-app
press 688 28
has 'asked for the share sheet' && ok "a click on Share (in the jailed app) asks for the sheet" || bad "share click: $(V "grep -h share-demo $LOG | tail -3 | tr '\n' ';'")"
V "grep -q '2 types, 2 targets for alice' $SLOG" && ok "the sheet spent the press and found alice's Photos and Scraps from their manifests" || bad "targets: $(V "tail -4 $SLOG | tr '\n' ';'")"
sleep 10
shot 2 jail-sheet
press 340 196
has 'share-demo: chosen' && ok "a click on Scraps: the app hears Chosen" || bad "choose: $(V "tail -3 $SLOG $LLOG | tr '\n' ';'")"
sleep $WAIT
started=$(V "grep 'dev.alice.scratch.org.test.scraps for alice' $LOG | tail -1")
case "$started" in *"dev.alice.scratch.org.test.notes"*) ok "portcullisd started Scraps for Notes' instance: ${started#portcullisd: }" ;; *) bad "start: $started ($(V "grep -i scraps $LOG $LLOG | tail -4 | tr '\n' ';'"))" ;; esac
pu=$(echo "$started" | sed -n 's/.*runs as atrium-app-\([0-9]*\).*/\1/p')
if [ -n "$pu" ] && [ "$pu" != "$NU" ] && [ "$pu" != 65022 ]; then ok "as its own uid $pu (Notes $NU, limend 65022)"; else bad "target uid [$pu] Notes [$NU]"; fi
has 'share-target-demo: got "Groceries: rice, lentils, coriander" 2 formats image/png=19 text/plain=35' \
  && ok "Scraps, in its jail, got the title, the picture by descriptor and the text" || bad "item: $(V "cat $logs | grep share-target-demo | tr '\n' ';'")"
shot 2 jail-target
press 588 452
has 'share-demo: accepted' && ok "a click on Keep: the app hears Accepted" || bad "keep: $(V "cat $logs | grep -E 'share-demo|share-target-demo' | tail -3 | tr '\n' ';'")"
shot 4 jail-done

echo "== clean up"
A "stop scratch" >/dev/null
V "pkill -f share-sheet; pkill -x limend; pkill -x portcullisd; pkill -x frescod; sleep 1; $GONE"
for f in $LOG $LLOG $FLOG $SLOG; do get $f "$OUT/$(basename $f)" 2>/dev/null; done
end
