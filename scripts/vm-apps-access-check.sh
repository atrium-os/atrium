#!/bin/sh
# Curia's Apps page (docs/spec/curia.md §13) in the cloud VM: portcullisd's
# Access, ApproveAccess and RemoveAccess, with frescod headless and real
# presses. Runs on the HOST after `sh scripts/cloud-vm.sh up`. Ends
# "apps-access: N failed".
#
#   sh scripts/vm-apps-access-check.sh [out-dir]
#
# Apps (all signed by the gate's publisher key, installed with opifex):
#   org.test.new        asks for the clipboard and notifications: alice's
#                       to allow
#   org.test.sys        asks for audio-control (restricted): the system's
#                       grant only
#   org.atrium.curia    Settings, trimmed to graphics + app-grants, granted
#                       by the system, launched for alice
#
#   - a caller that is not a launched app (root over nc): refused;
#   - Curia lists the three for alice;
#   - Allow on org.test.new: portcullisd spends the press with frescod and
#     writes alice's grant;
#   - a forged press (root's own token) can't allow anything;
#   - org.test.sys has no Allow; nothing is written for it;
#   - Remove access, then Remove: the grant is gone.
#   - Links (opening-links.md §4.4): New and Sys both take mailto: links;
#     the mailto row lists them, Use on one makes it alice's default
#     (links.toml), Ask each time takes it away; root can't do either.
#   - Share (share-sheet.md §4.4): New and Sys are share-targets; Hide on
#     New and Move up on Sys land in alice's share.toml; root can't.
#   - Between personas (personas.md §6): alice has a Work persona; Turn on
#     for "Personal may send links to Work" asks Sigillum to confirm it's
#     her (a stand-in answers, sigillum_stand_in) and writes the
#     [[crossings]] switch; refused, nothing is written; Turn off needs no
#     confirming; root can't turn one. "Personal may share to Work's apps"
#     turns on and off the same way.
#   - Adding School while Files is open relaunches it with School’s group.
# WX/WY: where frescod puts Curia's window (0 0 with no window manager).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/apps-access}
mkdir -p "$OUT"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
WX=${WX:-0}; WY=${WY:-0}

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "apps-access: 1 failed"; exit 1; }; }
[ -n "${NO_BUILD:-}" ] || {
    xb frescod "--bin frescod"
    xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex --example home_probe --example sigillum_stand_in"
    xb curia "--bin curia"
}

echo "=== install"
B=/var/tmp/apps-access
V "pkill -x portcullisd; pkill -x frescod; pkill -x curia; pkill -f sigillum_stand_in; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in frescod/target/$TARGET/release/frescod portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/examples/home_probe portcullis/target/$TARGET/release/examples/sigillum_stand_in curia/target/$TARGET/release/curia; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "apps-access: 1 failed"; exit 1; }
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
POL=/var/db/atrium/alice/policy.toml
NEW=org.test.new; SYS=org.test.sys; CURIA=org.atrium.curia; FILES=org.atrium.scrinium
# Other installed apps (other gates') are set aside while this runs, so the
# page lists only these three, and put back at the end.
ASIDE=/var/lib/atrium/apps-aside
V 'sh -s' <<EOS
set -e
if [ -d $ASIDE ]; then mv $ASIDE/* /var/lib/atrium/apps/ 2>/dev/null || true; rmdir $ASIDE; fi
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
mk() { # id name binary-in-B entry-name capabilities
  opifex uninstall \$1 >/dev/null 2>&1 || true
  O=$B/b-\$1; rm -rf \$O; mkdir -p \$O/bin; cp $B/\$3 \$O/bin/\$4
  printf '[app]\nid = "%s"\nname = "%s"\nversion = "1"\nentry = "bin/%s"\n\n[capabilities]\n%b\n%b' \$1 "\$2" \$4 "\$5" "\${6:-}" > \$O/atrium.toml
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
}
mk $NEW New home_probe stay_probe 'clipboard = true\nnotify = true' '\n[[links]]\nschemes = ["mailto"]\n\n[roles]\nimplements = ["share-target"]\n'
mk $SYS Sys home_probe stay_probe 'audio-control = true' '\n[[links]]\nschemes = ["mailto"]\n\n[roles]\nimplements = ["share-target"]\n'
rm -f /var/db/atrium/alice/links.toml /var/db/atrium/alice/share.toml
mkdir -p $ASIDE
for d in /var/lib/atrium/apps/*; do case \${d##*/} in $NEW|$SYS|$CURIA) ;; *) mv \$d $ASIDE/ ;; esac; done
mk $CURIA Settings curia curia 'graphics = "fresco"\napp-grants = true'
# Set the signed Files probe aside until the Personas test so it does not
# change any earlier Apps-page rows.
mkdir -p $B/files-probe/bin
cp $B/home_probe $B/files-probe/bin/stay_probe
printf '[app]\nid = "%s"\nname = "Files"\nversion = "1"\nentry = "bin/stay_probe"\n\n[capabilities]\nhome-files = true\n' $FILES > $B/files-probe/atrium.toml
openssl dgst -sha256 -sign /root/of-key.pem -out $B/files-probe/atrium.toml.sig $B/files-probe/atrium.toml
printf '[personas.work]\ncolour = "blue"\n' > $POL
daemon -f -o $B/sigillum.log $B/sigillum_stand_in $B/sigillum.sock
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG env PORTCULLISD_FRESCO_ACTIVATION=$B/activation.sock PORTCULLIS_SIGILLUM=$B/sigillum.sock /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
portcullis policy grant --system $SYS >/dev/null 2>&1
portcullis policy grant --system $CURIA >/dev/null 2>&1
EOS
[ $? = 0 ] && ok "portcullisd and the three apps installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "apps-access: $fails failed"; exit 1; }

echo "=== not a launched app"
R=$(V "printf '{\"op\":\"hello\",\"version\":1}\n{\"op\":\"access\"}\n{\"op\":\"approve_access\",\"app_id\":\"$NEW\",\"activation\":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]}\n' | nc -N -U $PSOCK")
case "$R" in *"not a launched app"*"not a launched app"*) ok "root over the socket: refused, both" ;; *) bad "root: $R" ;; esac

echo "=== frescod"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "apps-access: $fails failed"; exit 1; }
inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $B/inject.sock" >/dev/null; }
press() { inj "MOVE $(($1 + WX)) $(($2 + WY))" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-6}; }
shot() { sleep 4; get $B/screen.png "$OUT/$1.png" 2>/dev/null || get $B/screen "$OUT/$1.png" 2>/dev/null; [ -s "$OUT/$1.png" ] && echo "  screenshot $OUT/$1.png" || echo "  (no screenshot for $1)"; }
count() { V "grep -c '$1' $LOG" | tr -d ' '; }

echo "=== Settings, the Apps page"
V "daemon -f -o $B/curia.log su -m alice -c 'portcullis launch --no-prompt $CURIA'"
i=0; while [ $i -lt 60 ] && ! V "grep -q 'launching $CURIA as uid' $LOG"; do sleep 2; i=$((i+2)); done
sleep 20
shot apps-0
press 70 263
i=0; while [ $i -lt 40 ] && [ "$(count "access -> $CURIA")" = 0 ]; do sleep 2; i=$((i+2)); done
L=$(V "grep 'access -> $CURIA' $LOG | tail -1")
case "$L" in *"for alice: "*" app(s)"*) ok "Curia lists the apps for alice ($(echo "$L" | sed 's/.*: //'))" ;; *) bad "no list: $L / $(V "tail -5 $LOG")" ;; esac
shot apps-1

echo "=== Allow"
# The first row (New): Allow is its rightmost button.
press 906 124
i=0; while [ $i -lt 30 ] && [ "$(count "alice allowed $NEW")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "alice allowed $NEW (from $CURIA)")" = 1 ] && ok "a click on Allow: alice's grant" || bad "no grant: $(V "grep -i 'access\|REFUSED' $LOG | tail -3")"
V "grep -q 'org.test.new' $POL" && ok "written to her policy" || bad "policy: $(V "cat $POL")"
shot apps-2
V "daemon -f -o $B/new.log su -m alice -c 'portcullis launch --no-prompt $NEW'"
i=0; while [ $i -lt 30 ] && ! V "grep -q 'launching $NEW as uid' $LOG"; do sleep 2; i=$((i+2)); done
V "grep -q 'launching $NEW as uid' $LOG" && ok "New now launches without asking" || bad "launch: $(V "tail -3 $LOG")"
V "pkill -f stay_probe" >/dev/null 2>&1

echo "=== the system's app"
# The third row (Sys): no Allow; a press there allows nothing.
press 906 244
[ "$(count "allowed $SYS")" = 0 ] && ok "Sys has no Allow" || bad "Sys was allowed"
V "grep -q 'org.test.sys' $POL" && bad "Sys in alice's policy" || ok "nothing written for Sys"
shot apps-3

echo "=== Remove"
press 906 124
shot apps-4
press 800 171
i=0; while [ $i -lt 30 ] && [ "$(count "alice removed $NEW")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "alice removed $NEW")" = 1 ] && ok "Remove access, then Remove: gone" || bad "not removed: $(V "grep -i 'access\|REFUSED' $LOG | tail -3")"
V "grep -q 'org.test.new' $POL" && bad "still in her policy" || ok "not in her policy"
shot apps-5

echo "=== Links"
LT=/var/db/atrium/alice/links.toml
R=$(V "printf '{\"op\":\"hello\",\"version\":1}\n{\"op\":\"links\"}\n' | nc -N -U $PSOCK")
case "$R" in *"not a launched app"*) ok "root asking for the links: refused" ;; *) bad "root links: $R" ;; esac
L=$(V "grep 'links -> $CURIA' $LOG | tail -1")
case "$L" in *"for alice: 1 scheme(s)"*) ok "Curia's Links section: one scheme (mailto)" ;; *) bad "links: $L" ;; esac
press 400 244                                    # close Sys's details
press ${LX:-400} ${LY:-356}                      # the mailto row
shot links-0
press ${UX:-900} ${UY:-406}                      # Use, on New
i=0; while [ $i -lt 30 ] && [ "$(count "default for mailto links is now $NEW")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "default for mailto links is now $NEW (from $CURIA)")" = 1 ] && ok "Use on New: alice's mailto: default" || bad "no default: $(V "grep -i 'link\|REFUSED' $LOG | tail -3")"
V "grep -qx 'mailto = \"$NEW\"' $LT" && ok "written to her links.toml" || bad "links.toml: $(V "cat $LT 2>&1")"
shot links-1
press ${AX:-900} ${AY:-478}                      # Ask, for Ask each time
i=0; while [ $i -lt 30 ] && [ "$(count "mailto links is now ask each time")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "default for mailto links is now ask each time")" = 1 ] && ok "Ask each time: the default is gone" || bad "not cleared: $(V "grep -i 'link\|REFUSED' $LOG | tail -3")"
V "grep -q mailto $LT" && bad "still in links.toml" || ok "not in her links.toml"
shot links-2

echo "=== Share"
ST=/var/db/atrium/alice/share.toml
R=$(V "printf '{\"op\":\"hello\",\"version\":1}\n{\"op\":\"share_targets\"}\n' | nc -N -U $PSOCK")
case "$R" in *"not a launched app"*) ok "root asking for the share targets: refused" ;; *) bad "root share targets: $R" ;; esac
L=$(V "grep 'share targets -> $CURIA' $LOG | tail -1")
case "$L" in *"for alice: 2"*) ok "Curia's Share section: New and Sys" ;; *) bad "share targets: $L" ;; esac
press ${LX:-400} ${LY:-356}                      # close the mailto row
shot share-0
press ${HX:-906} ${HY:-${SHY:-500}}              # Hide, on New (the first row)
i=0; while [ $i -lt 30 ] && [ "$(count "alice's share sheet")" = 0 ]; do sleep 2; i=$((i+2)); done
V "grep -q 'hidden = \[\"$NEW\"\]' $ST" && ok "Hide on New: off alice's share sheet (share.toml)" || bad "hide: $(V "cat $ST 2>&1; grep -i 'share\|REFUSED' $LOG | tail -2")"
shot share-1
press ${MX:-800} ${MY:-$(( ${SHY:-500} + 52 ))}   # Move up, on Sys (the second row)
i=0; while [ $i -lt 30 ] && [ "$(count "alice's share sheet")" -lt 2 ]; do sleep 2; i=$((i+2)); done
V "tr -d ' \n' < $ST" | grep -q "order=\[\"$SYS\",\"$NEW\",*\]" && ok "Move up on Sys: first, New still hidden" || bad "move up: $(V "cat $ST 2>&1")"
shot share-2
R=$(V "printf '{\"op\":\"hello\",\"version\":1}\n{\"op\":\"set_share_targets\",\"order\":[],\"hidden\":[],\"activation\":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]}\n' | nc -N -U $PSOCK")
case "$R" in *"not a launched app"*) ok "root can't change it" ;; *) bad "root set share: $R" ;; esac

echo "=== Between personas"
R=$(V "printf '{\"op\":\"hello\",\"version\":1}\n{\"op\":\"set_crossing\",\"from\":\"personal\",\"to\":\"work\",\"kind\":\"links\",\"on\":true,\"activation\":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]}\n' | nc -N -U $PSOCK")
case "$R" in *"not a launched app"*) ok "root turning a switch: refused" ;; *) bad "root crossing: $R" ;; esac
L=$(V "grep 'crossings -> $CURIA' $LOG | tail -1")
case "$L" in *"for alice: 1 persona(s), 0 switch(es) on"*) ok "Curia's Between personas section: Work, all off" ;; *) bad "crossings: $L" ;; esac
press ${LX:-400} ${LY:-356}                      # close the mailto row
press 70 301                                     # the Personas page
shot persona-0
press ${CX:-880} ${CY:-484}                      # Turn on: Personal may send links to Work
i=0; while [ $i -lt 30 ] && [ "$(count "switch for links from personal to work is now on")" = 0 ]; do sleep 2; i=$((i+2)); done
V "grep -q 'verify alice for Crossing { from: \"personal\", to: \"work\", kind: \"links\" }: confirmed' $B/sigillum.log" && ok "Sigillum asked to confirm it's alice" || bad "no verify: $(V "cat $B/sigillum.log")"
[ "$(count "switch for links from personal to work is now on (from $CURIA, verified)")" = 1 ] && ok "confirmed: the switch is on" || bad "not on: $(V "grep -i 'crossing\|switch\|REFUSED' $LOG | tail -3")"
V "grep -A3 '^\[\[crossings\]\]' $POL | grep -q links" && ok "written to her policy" || bad "policy: $(V "cat $POL")"
shot persona-1
V "touch $B/sigillum.sock.refuse"
press ${CX:-880} ${KY:-524}                      # Turn on: clipboard, refused this time
i=0; while [ $i -lt 30 ] && ! V "grep -q 'kind: \"clipboard\" }: refused' $B/sigillum.log"; do sleep 2; i=$((i+2)); done
sleep 3
[ "$(count "for clipboard from personal to work is now on")" = 0 ] && ok "not confirmed: the clipboard switch stays off" || bad "clipboard on without confirming"
V "grep -q clipboard $POL" && bad "clipboard in her policy" || ok "nothing written for it"
shot persona-2
V "rm -f $B/sigillum.sock.refuse"
press ${CX:-880} ${CY:-484}                      # Turn off: links
i=0; while [ $i -lt 30 ] && [ "$(count "from personal to work is now off")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "switch for links from personal to work is now off")" = 1 ] && ok "Turn off: off, with no confirming" || bad "not off: $(V "grep -i 'switch\|REFUSED' $LOG | tail -3")"
[ "$(V "grep -c 'kind: \"links\"' $B/sigillum.log" | tr -d ' ')" = 1 ] && ok "Sigillum was asked once for links" || bad "sigillum: $(V "cat $B/sigillum.log")"
V "grep -q links $POL" && bad "still in her policy: $(V "cat $POL")" || ok "not in her policy"
shot persona-3
press ${CX:-880} ${SY:-604}                      # Turn on: Personal may share to Work's apps
i=0; while [ $i -lt 30 ] && [ "$(count "switch for share from personal to work is now on")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "switch for share from personal to work is now on (from $CURIA, verified)")" = 1 ] && ok "the share switch: on, confirmed" || bad "share switch: $(V "grep -i 'switch\|REFUSED' $LOG | tail -3")"
V "grep -A3 '^\[\[crossings\]\]' $POL | grep -q share" && ok "written to her policy" || bad "policy: $(V "cat $POL")"
shot persona-share
press ${CX:-880} ${SY:-604}                      # and off again
i=0; while [ $i -lt 30 ] && [ "$(count "switch for share from personal to work is now off")" = 0 ]; do sleep 2; i=$((i+2)); done
V "grep -q share $POL" && bad "share still in her policy" || ok "the share switch: off again"

echo "=== Personas: colour, add, remove"
key() { for k in "$@"; do inj "KEY $k 1" "KEY $k 0"; done; sleep 2; }
press ${GX:-372} ${GY:-220}                      # Work's colour: green
i=0; while [ $i -lt 20 ] && [ "$(count "persona work is now Some(\"green\")")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "persona work is now Some(\"green\")")" = 1 ] && ok "Work is green now" || bad "colour: $(V "grep -i 'persona\|REFUSED' $LOG | tail -3")"
V "grep -A1 '^\[personas.work\]' $POL | grep -q green" && ok "in her policy" || bad "policy: $(V "cat $POL")"
V "mv $B/files-probe /var/lib/atrium/apps/$FILES
   daemon -f -o $B/files.log -u alice /usr/local/bin/portcullis launch --no-prompt $FILES"
i=0; while [ $i -lt 30 ] && ! V "grep -q 'home-probe:.*persona_made=ok' $B/files.log"; do sleep 2; i=$((i+2)); done
V "grep -q 'home-probe:.*persona_made=ok' $B/files.log" && ok "Files probe is running before the add" || bad "Files launch: $(V "tail -5 $B/files.log")"
press ${FX:-300} ${FY:-326}                      # the name field
key 0x16 0x06 0x0b 0x12 0x12 0x0f                # "school"
press ${AX2:-484} ${AY2:-325}                    # Add
i=0; while [ $i -lt 20 ] && [ "$(count "persona school is now")" = 0 ]; do sleep 2; i=$((i+2)); done
[ "$(count "persona school is now Some(\"blue\")")" = 1 ] && ok "School added, in the first free colour" || bad "add: $(V "grep -i 'persona\|REFUSED' $LOG | tail -3")"
i=0; while [ $i -lt 30 ] && ! V "grep -q 'restarting Files for alice' $LOG && grep 'home-probe:' $B/files.log | tail -1 | grep -q 'personas=\[school,.*persona_made=ok'"; do sleep 2; i=$((i+2)); done
if V "test \$(grep -c 'home-probe:' $B/files.log) -eq 2 &&
      grep -q 'restarting Files for alice' $LOG &&
      grep 'home-probe:' $B/files.log | tail -1 | grep -q 'personas=\[school,.*persona_made=ok' &&
      test \"\$(stat -f %Sg /home/alice/.atrium/personas/school/Documents/probe.txt)\" = files-alice-school"; then
    ok "Files restarted once and wrote in School's Documents with School's group"
else
    bad "Files restart: $(V "tail -5 $B/files.log; tail -8 $LOG")"
fi
shot persona-4
V "mkdir -p /home/alice/.atrium/persona-apps/work/org.test.new/var /home/alice/.atrium/persona-apps/school && touch /home/alice/.atrium/persona-apps/work/org.test.new/var/data /home/alice/.atrium/persona-apps/school/org.test.new.img"
press ${CX:-880} ${RY:-288}                      # Work's Remove…
shot persona-5
press ${DX:-330} ${DY:-379}                      # Remove Work
i=0; while [ $i -lt 30 ] && [ "$(count "removed persona work")" = 0 ]; do sleep 2; i=$((i+2)); done
V "grep -q 'verify alice for RemovePersona { name: \"work\" }: confirmed' $B/sigillum.log" && ok "Sigillum asked to confirm it's alice" || bad "no verify: $(V "tail -3 $B/sigillum.log")"
[ "$(count "removed persona work (from $CURIA, verified): ")" = 1 ] && ok "Work removed, its app folder deleted" || bad "remove: $(V "grep -i 'persona\|REFUSED' $LOG | tail -3")"
V "test -e /home/alice/.atrium/persona-apps/work/org.test.new" && bad "Work's folder is still there" || ok "Work's folder is gone"
V "test -e /home/alice/.atrium/persona-apps/school/org.test.new.img" && ok "School's is kept" || bad "School's folder went too"
V "grep -q 'personas.work' $POL" && bad "Work still in her policy" || ok "Work out of her policy"
shot persona-6

V "pkill -TERM -j org_atrium_scrinium -x stay_probe; pkill -x curia; pkill -f sigillum_stand_in" >/dev/null 2>&1
# Its own test apps go (they take mailto:, which other gates count on).
V "opifex uninstall $NEW >/dev/null 2>&1; opifex uninstall $SYS >/dev/null 2>&1; if test -d /var/lib/atrium/apps/$FILES; then mv /var/lib/atrium/apps/$FILES $B/files-probe; fi; mv $ASIDE/* /var/lib/atrium/apps/ 2>/dev/null; rmdir $ASIDE"
echo "apps-access: $fails failed"
