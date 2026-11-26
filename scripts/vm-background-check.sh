#!/bin/sh
# Background apps B2 (docs/spec/background-apps.md §3.3): sign-in start,
# supervision and sign-out, in the dev VM. Runs on the HOST; ends
# "background: N ok, M failed".
#
#   ARCH=amd64 sh scripts/cloud-vm.sh up      # or the Mac VM, with its OK
#   sh scripts/deploy-portcullis.sh           # once: jaild with allow_rctl
#   ARCH=amd64 sh scripts/vm-background-check.sh
#
# The session launcher is played by a client running as _ostiarius (uid
# 50093, the uid its services.d manifest names: that is what portcullisd
# grants session_launch by), and the seat file names the person, as
# ostiarius writes it at login. ostiarius's own part (StartBackground after
# the session components, once per session) is its unit test.
#
#   1. bgprobe (start = "sign-in", allowed in Personal), bgnope (sign-in,
#      not allowed) and bgmanual (start = "manual", allowed) are installed;
#   2. StartBackground from root is refused; with no live session refused;
#   3. from _ostiarius: bgprobe runs with no press, under its rctl limits,
#      and bgnope and bgmanual don't;
#   4. a second StartBackground starts nothing more;
#   5. kill -9 restarts it; a third crash in a minute pauses it;
#   6. the person's grant revoked during the pause: it doesn't come back,
#      and a StartBackground after the grant is back doesn't start it;
#   7. bgmanual opened by the person is supervised: kill -9 restarts it;
#   8. sign-out: nothing comes back, the apps are gone and the overlays
#      unmounted; the next session starts bgprobe again;
#   9. the bar's list (§3.2), asked as the person's forum-bar session
#      component: refused to the person's own uid; lists bgprobe; Quit ends
#      it, it isn't restarted, and StartBackground doesn't bring it back;
#      "Don't open at sign-in" turns the person's switch off.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=${ARCH:-aarch64}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
. "$ROOT/scripts/lib/gate.sh"
gate_begin background 33
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
TGT=${CARGO_TARGET_DIR:-$ROOT/portcullis/target}

B=/var/tmp/background
PSOCK=/atrium/sockets/portcullis/portcullis.sock
SEAT=/atrium/sockets/ostiarius/active-session
LOG=$B/portcullisd.log
U=bgcheck
HU=/home/$U
APPS=/var/lib/atrium/apps
BG=org.test.bgprobe; NOPE=org.test.bgnope; MAN=org.test.bgmanual

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p opifex \
    --bin portcullisd --bin portcullis --bin opifex --example home_probe) \
    || gate_abort "cross-build failed"

echo "=== install"
V "sysctl -n kern.racct.enable" | grep -q 1 || gate_abort "kern.racct.enable is 0 (cloud-vm.sh sets it at its first boot)"
V "grep -q '^allow_rctl *= *true' /etc/atrium/jaild.policy.toml && pgrep -qf atrium-jaild" \
    || gate_abort "jaild with allow_rctl = true is not running (sh scripts/deploy-portcullis.sh)"
V "pkill -f bg_stay_probe; pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B" >/dev/null 2>&1
for b in release/portcullisd release/portcullis release/opifex release/examples/home_probe; do
    scp -q $K -P "$VM_SSH_PORT" "$TGT/$TARGET/$b" root@localhost:$B/ || gate_abort "copy $b"
done
scp -q $K -P "$VM_SSH_PORT" "$ROOT/portcullis/ostiarius/etc/services.d/45-atrium-ostiarius.toml" root@localhost:$B/ \
    || gate_abort "copy ostiarius's manifest"

V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
id _ostiarius >/dev/null 2>&1 || pw useradd _ostiarius -u 50093 -d /nonexistent -s /usr/sbin/nologin
install -d -o _ostiarius -g _ostiarius -m 0755 /atrium/sockets/ostiarius
# Its manifest is what grants _ostiarius session_launch; enabled = false so
# the bootstrap starts no ostiarius of its own.
sed 's/^enabled *= *true/enabled       = false/' $B/45-atrium-ostiarius.toml > /etc/atrium/services.d/45-atrium-ostiarius.toml
id $U >/dev/null 2>&1 || pw useradd $U -u 1077 -m -d $HU -s /bin/sh
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
mkdir -p /atrium/sockets/clipboard
mk() { # id start
  opifex uninstall \$1 >/dev/null 2>&1 || true
  O=$B/b-\$1; mkdir -p \$O/bin; cp $B/home_probe \$O/bin/bg_stay_probe
  printf '[app]\nid = "%s"\nname = "%s"\nversion = "1"\nentry = "bin/bg_stay_probe"\n\n[capabilities]\nclipboard = true\n\n[background.resident]\nstart = "%s"\n\n[resources]\nmemory = "96M"\ncpu = 40\n' \$1 \$1 \$2 > \$O/atrium.toml
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
}
mk $BG sign-in; mk $NOPE sign-in; mk $MAN manual
h() { echo "sha256:\$(sha256 -q $APPS/\$1/atrium.toml)"; }
mkdir -p /var/db/atrium/$U
# The person's background switches, as Settings writes them: bgprobe and
# bgmanual in Personal; bgnope never.
cat > /var/db/atrium/$U/policy.toml <<POL
[background."$BG"]
manifest_hash = "\$(h $BG)"
granted_at = "2026-11-26T00:00:00Z"
personas = ["personal"]

[background."$MAN"]
manifest_hash = "\$(h $MAN)"
granted_at = "2026-11-26T00:00:00Z"
personas = ["personal"]
POL
: > $SEAT
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd
i=0; while [ \$i -lt 30 ] && ! [ -S $PSOCK ]; do sleep 0.5; i=\$((i+1)); done
test -S $PSOCK; chmod 666 $PSOCK
for a in $BG $NOPE $MAN; do su -m $U -c "portcullis policy grant \$a" >/dev/null; done
EOS
[ $? = 0 ] && ok "portcullisd and the three apps installed, the person's grants written" \
    || gate_abort "setup: $(V "tail -3 $LOG")"

# say AS JSON: one request after the hello, as uid AS; prints the reply line.
say() {
    printf '{"op":"hello","version":1}\n%s\n' "$2" | V "cat > $B/req.json && chmod 644 $B/req.json"
    V "su -m $1 -c 'nc -N -U $PSOCK < $B/req.json'" 2>/dev/null | tail -1 | tr -d '\r'
}
start() { say _ostiarius '{"op":"start_background","owner":"'$U'"}'; }
wait_for() { # cmd... until it prints non-empty, 60 s
    i=0; while [ $i -lt 60 ]; do r=$("$@"); [ -n "$r" ] && [ "$r" != 0 ] && { echo "$r"; return; }; sleep 1; i=$((i+1)); done
}
jid_of() { V "jls -n jid name | awk -F'[ =]' '\$4 ~ /$(echo $1 | tr . _)\$/ {print \$2}'" 2>/dev/null | tr -d '\r' | head -1; }
pid_in() { j=$(jid_of $1); [ -n "$j" ] && V "pgrep -j $j bg_stay_probe" 2>/dev/null | tr -d '\r' | head -1; }
count() { V "grep -c -F '$1' $LOG" 2>/dev/null | tr -d '\r '; }

echo "=== 2. refusals"
case "$(say root '{"op":"start_background","owner":"'$U'"}')" in
    *session_launch.denied*) ok "StartBackground from root: refused" ;;
    *) bad "from root: $(say root '{"op":"start_background","owner":"'$U'"}')" ;;
esac
case "$(start)" in
    *"no live session"*) ok "with no live session: refused" ;;
    *) bad "with no seat: $(start)" ;;
esac

echo "=== 3. sign-in"
V "printf '%s\n' $U > $SEAT"
check "StartBackground from _ostiarius" '{"kind":"ok"}' "$(start)"
P1=$(wait_for pid_in $BG)
[ -n "$P1" ] && ok "bgprobe runs, with no press (pid $P1)" || bad "bgprobe didn't start: $(V "grep background $LOG | tail -3")"
JN=$(echo $BG | tr . _)
R=$(V "rctl | grep 'jail:$JN:'" 2>/dev/null | tr -d '\r')
case "$R" in *pcpu:deny=40*memoryuse:sigkill=100663296*|*memoryuse:sigkill=100663296*pcpu:deny=40*) ok "under its [resources]: 96M, 40% of a core" ;;
    *) bad "rctl for $JN: [$R]" ;; esac
sleep 3
check "bgnope (not allowed) didn't start" "" "$(pid_in $NOPE)"
check "bgmanual (manual) didn't start" "" "$(pid_in $MAN)"

echo "=== 4. a second StartBackground"
check "answers Ok" '{"kind":"ok"}' "$(start)"
sleep 3
check "…and starts nothing more" "1" "$(V "jls -n name | grep -c 'name=$JN'" | tr -d '\r ')"
check "…and said so" 1 "$(count "background: 0 app(s) started at $U")"

echo "=== 5. crashes"
V "kill -9 $P1"
P2=$(i=0; while [ $i -lt 60 ]; do p=$(pid_in $BG); [ -n "$p" ] && [ "$p" != "$P1" ] && { echo $p; break; }; sleep 1; i=$((i+1)); done)
[ -n "$P2" ] && ok "kill -9 restarted it (pid $P2)" || bad "no restart after kill -9: $(V "grep background $LOG | tail -3")"
V "kill -9 $P2"
P3=$(i=0; while [ $i -lt 60 ]; do p=$(pid_in $BG); [ -n "$p" ] && [ "$p" != "$P2" ] && { echo $p; break; }; sleep 1; i=$((i+1)); done)
[ -n "$P3" ] && ok "a second crash restarted it (pid $P3)" || bad "no restart after the second crash"
V "kill -9 $P3"
i=0; while [ $i -lt 30 ] && [ "$(count "$BG for $U crashed too often")" = 0 ]; do sleep 1; i=$((i+1)); done
check "a third crash in a minute pauses it" 1 "$(count "$BG for $U crashed too often")"
sleep 5
check "…and nothing runs" "" "$(pid_in $BG)"

echo "=== 6. revoked during the pause"
V "su -m $U -c 'portcullis policy revoke $BG'" >/dev/null 2>&1
i=0; while [ $i -lt 90 ] && [ "$(count "$BG for $U stopped: no longer allowed")" = 0 ]; do sleep 2; i=$((i+2)); done
check "after the pause: not restarted, no longer allowed" 1 "$(count "$BG for $U stopped: no longer allowed")"
check "…and nothing runs" "" "$(pid_in $BG)"
V "su -m $U -c 'portcullis policy grant $BG'" >/dev/null 2>&1
start >/dev/null; sleep 5
check "allowed again, a StartBackground in the same session doesn't start it" "" "$(pid_in $BG)"

echo "=== 7. a manual app the person opens"
V ": > $B/manual.log; chmod 666 $B/manual.log; daemon -f -o $B/manual.log su -m $U -c 'portcullis launch --no-prompt $MAN'"
M1=$(wait_for pid_in $MAN)
[ -n "$M1" ] && ok "bgmanual opened (pid $M1)" || bad "bgmanual didn't open: $(V "tail -3 $B/manual.log")"
V "kill -9 $M1"
M2=$(i=0; while [ $i -lt 60 ]; do p=$(pid_in $MAN); [ -n "$p" ] && [ "$p" != "$M1" ] && { echo $p; break; }; sleep 1; i=$((i+1)); done)
[ -n "$M2" ] && ok "…supervised: kill -9 restarted it (pid $M2)" || bad "bgmanual not restarted: $(V "grep background $LOG | tail -3")"

echo "=== 8. sign-out"
check "SessionEnded" '{"kind":"ok"}' "$(say _ostiarius '{"op":"session_ended","user":"'$U'"}')"
V ": > $SEAT"
sleep 10
check "nothing came back" "" "$(pid_in $MAN)$(pid_in $BG)"
check "…and the kill was not a failure" 1 "$(count "$MAN for $U stopped: signing out")"
check "the apps are gone and the overlays unmounted" 1 "$(V "grep -c '$U logging out: .*overlays unmounted' $LOG" | tr -d '\r ')"
check "…and nothing is mounted from ~/.atrium" 0 "$(V "mount -p | grep -c '$HU/.atrium/'" | tr -d '\r ')"
V "printf '%s\n' $U > $SEAT"
start >/dev/null
P4=$(wait_for pid_in $BG)
[ -n "$P4" ] && ok "the next session starts bgprobe again (pid $P4)" || bad "not started in the next session"

echo "=== 9. the bar's list"
BARUID=62998
V "id _bgbar >/dev/null 2>&1 || pw useradd _bgbar -u $BARUID -d /nonexistent -s /usr/sbin/nologin; \
   printf '%s\n' '$BARUID $U org.atrium.forum-bar session' >> /var/run/atrium/app-registry"
case "$(say $U '{"op":"background_apps"}')" in
    *cap.background.denied*) ok "the list is refused to the person's own uid" ;;
    *) bad "the person's own uid got the list" ;;
esac
L=$(say _bgbar '{"op":"background_apps"}')
case "$L" in
    *'"kind":"background_apps"'*'"app_id":"'$BG'"'*'"at_sign_in":true'*) ok "the bar lists bgprobe, opening at sign-in" ;;
    *) bad "bar list: $L" ;;
esac
check "Quit" '{"kind":"ok"}' "$(say _bgbar '{"op":"background_quit","app_id":"'$BG'"}')"
sleep 10
check "…it ends and isn't restarted" "" "$(pid_in $BG)"
start >/dev/null; sleep 5
check "…and a StartBackground (still allowed) doesn't bring it back" "" "$(pid_in $BG)"
check "Don't open at sign-in" '{"kind":"ok"}' "$(say _bgbar '{"op":"background_off","app_id":"'$BG'"}')"
check "…the switch is off in the person's policy" 0 \
    "$(V "grep -c -F '[background.\"$BG\"]' /var/db/atrium/$U/policy.toml" | tr -d '\r ')"
V "sed -i '' '/^$BARUID $U org.atrium.forum-bar session\$/d' /var/run/atrium/app-registry"

echo "=== clean up"
say _ostiarius '{"op":"session_ended","user":"'$U'"}' >/dev/null
V ": > $SEAT; sleep 3; for a in $BG $NOPE $MAN; do opifex uninstall \$a >/dev/null 2>&1; done; pkill -x portcullisd; rm -f /etc/atrium/services.d/45-atrium-ostiarius.toml" >/dev/null 2>&1
gate_end
