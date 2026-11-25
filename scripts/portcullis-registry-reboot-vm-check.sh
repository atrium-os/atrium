#!/bin/sh
# Launch registry across a reboot (portcullis 17f88fb26 + the in-place
# restore fix). Guest side; runs after scripts/mail-at-rest-vm-check.sh on
# the same guest, reusing what it left: the staged binaries in A5 (the gate's
# a5 directory), Mail installed and signed with that run's key, a5work's
# grants, and the bindings in /var/db/atrium/app-registry.
#
#   sh portcullis-registry-reboot-vm-check.sh A5 before   # record the bindings
#   (reboot)
#   sh portcullis-registry-reboot-vm-check.sh A5 after    # restore, relaunch, a second app
set -u
d=$1; phase=$2
. "$d/lib/gate.sh"
kept=/var/db/atrium/app-registry
live=/var/run/atrium/app-registry
out=/var/tmp/registry-reboot
app=org.atrium.mail
mkdir -p "$out"

if [ "$phase" = before ]; then
    gate_begin registry-before 3
    check "the kept registry exists" yes "$(test -s $kept && echo yes)"
    cp "$kept" "$out/kept-before"
    cp "$live" "$out/live-before"
    check "live and kept registries hold the same bindings" "" "$(diff "$out/kept-before" "$out/live-before")"
    # a5work's personal and Work Mail
    w=$(awk -v a=$app '$2 == "a5work" && $3 == a && / persona=work/ {print $1}' "$kept" | tail -1)
    p=$(awk -v a=$app '$2 == "a5work" && $3 == a && !/ persona=/ {print $1}' "$kept" | tail -1)
    echo "work=$w personal=$p" | tee "$out/uids-before"
    check "a5work has a Work and a personal Mail binding" yes "$([ -n "$w" ] && [ -n "$p" ] && echo yes)"
    gate_end
    exit
fi

gate_begin registry-after 11
say() { printf '\n=== %s\n' "$*"; }
. "$out/uids-before" 2>/dev/null || true
w0=${work:-}; p0=${personal:-}
say "after the boot"
check "the boot cleared the live registry" no "$(test -s $live && echo yes || echo no)"
check "kept registry: root 0600" "0 600" "$(stat -f '%u %Lp' $kept)"
# /var/db/atrium also holds each person's policy folder, so it is root's
# 0755: no one else can create, rename or remove the registry in it.
check "kept registry directory: root's, writable by root only" "0 755" "$(stat -f '%u %Lp' $(dirname $kept))"
check "the kept registry survived the boot unchanged" "" "$(diff "$out/kept-before" $kept)"

say "portcullisd starts the way atrium-portcullisd-bootstrap leaves it: an empty live file"
for s in atrium-jaild; do service "$s" onestatus >/dev/null 2>&1 || service "$s" onestart >/dev/null 2>&1; done
install -d -m 0755 /var/run/atrium
[ -e $live ] || install -m 0644 -o root -g wheel /dev/null $live
ino=$(stat -f %i $live)
pkill -x portcullisd 2>/dev/null; sleep 1
s=/atrium/sockets/portcullis/portcullis.sock
install -d /atrium/sockets/portcullis
rm -f "$s" "$out/portcullisd.log"   # the last run's socket survives the boot
daemon -f -o "$out/portcullisd.log" "$d/portcullisd" --socket "$s"
n=0; while [ $n -lt 40 ] && [ ! -S $s ]; do sleep .5; n=$((n + 1)); done
chmod 666 $s
check "portcullisd logs 'launch registry restored'" 1 "$(grep -c 'launch registry restored from /var/db/atrium/app-registry' "$out/portcullisd.log")"
check "the live registry holds the kept bindings" "" "$(diff $kept $live)"
check "restored in place (the inode the services mounted)" "$ino" "$(stat -f %i $live)"

say "the same Mail gets the same uids"
launched() { # NAME: the uid in portcullisd's 'launching' line for this launch
    grep "launching $1 as uid" "$out/portcullisd.log" | tail -1 | sed 's/.* as uid \([0-9]*\).*/\1/'
}
timeout 30 su -m a5work -c "$d/portcullis launch $app" >"$out/launch-personal.log" 2>&1
p1=$(launched $app)
check "a5work's personal Mail: the uid it had before the boot" "$p0" "$p1"
timeout 30 su -m a5work -c "$d/portcullis launch $app --persona work" >"$out/launch-work.log" 2>&1
w1=$(grep "launching $app as uid" "$out/portcullisd.log" | grep 'persona=work' | tail -1 | sed 's/.* as uid \([0-9]*\).*/\1/')
check "a5work's Work Mail: the uid it had before the boot" "$w0" "$w1"
pkill -j org_atrium_mail -x atrium-mail 2>/dev/null; pkill -j org_atrium_mail__p_work -x atrium-mail 2>/dev/null

say "a different app gets a fresh uid"
other=org.atrium.regcheck
sign=$(mktemp -d /var/tmp/regsign.XXXXXX); chmod 700 "$sign"
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$sign/key" 2>/dev/null
openssl pkey -in "$sign/key" -pubout -out /etc/atrium/publishers/registry-reboot-check.pem 2>/dev/null
rm -rf "$sign/bundle"; cp -R "$d/bundle" "$sign/bundle"; rm -f "$sign/bundle/atrium.sig" "$sign/bundle"/*.sig
sed "s/^id *= *\"$app\"/id = \"$other\"/" "$d/mail.atrium.toml" >"$sign/bundle/atrium.toml"
grep -q "\"$other\"" "$sign/bundle/atrium.toml" || echo "note: app id not rewritten"
"$d/opifex" pack "$sign/bundle" --sign "$sign/key" >"$out/opifex.log" 2>&1
rm -rf "/var/lib/atrium/apps/$other"
"$d/opifex" install "$sign/bundle" >>"$out/opifex.log" 2>&1
su -m a5work -c "$d/portcullis policy grant $other" >"$out/grant.log" 2>&1
timeout 30 su -m a5work -c "$d/portcullis launch $other" >"$out/launch-other.log" 2>&1
o1=$(launched $other)
echo "other=$o1"
check "the other app launched with a uid" yes "$([ -n "$o1" ] && echo yes)"
check "that uid was bound to nothing before" 0 "$(awk -v u="$o1" '$1 == u' "$out/kept-before" | wc -l | tr -d ' ')"
pkill -j org_atrium_regcheck 2>/dev/null
rm -rf "$sign" "/var/lib/atrium/apps/$other" /etc/atrium/publishers/registry-reboot-check.pem
cp $live "$out/live-after"; cp $kept "$out/kept-after"
gate_end
