#!/bin/sh
# Run from a fresh, staged directory on a disposable arm64 desktop image.
# The host writes the randomly chosen argument to .isolated-gate on its clone.
set -eu
D=$(pwd)
test -n "$1"
test "$(cat .isolated-gate)" = "$1"
test "$(uname -s)/$(uname -m)" = FreeBSD/arm64
app=org.atrium.calendar
person=calfixture
home=/home/$person
reg=/var/run/atrium/app-registry
fsock=/atrium/sockets/fresco/fresco.sock
psock=/atrium/sockets/portcullis/portcullis.sock
launch() {
    daemon -f -o "$D/$1.log" su -m "$person" -c "$D/forum-dock launch $app --persona $1"
}
wait_ui() {
    for i in $(jot 35); do
        if pgrep -j "$1" -x atrium-calendar >/dev/null 2>&1; then return; fi
        sleep 1
    done
    cat "$2.log" portcullis.log >&2
    return 1
}
enter() {
    inj 'MOVE 220 143' 'BTN 1 1' 'BTN 1 0'
    sleep 1
    for code in "$@"; do inj "KEY $code 1 0" "KEY $code 0 0"; done
    inj 'MOVE 205 184' 'BTN 1 1' 'BTN 1 0'
}
inj() { printf '%s\n' "$@" | nc -w 3 -N -U inject.sock; }
saved() {
    for i in $(jot 15); do
        if grep -qs "\"title\":\"$2\"" "$1/calendar.json"; then return; fi
        sleep 1
    done
    echo "Missing synthetic entry $2" >&2
    return 1
}

test ! -e "$home"
test ! -e "/var/lib/atrium/apps/$app"
for bin in portcullisd atrium-jaild opifex portcullis; do
    install -m 0555 "$bin" "/usr/local/bin/$bin"
done
install -m 0644 jaild.policy.toml /etc/atrium/jaild.policy.toml
service atrium-jaild restart
pw useradd "$person" -m -d "$home" -s /bin/sh
sh calendar-bundle.sh ./atrium-calendar ./fastid ./atrium.toml /root/of-key.pem
mkdir -p /atrium/sockets/portcullis /atrium/sockets/fresco
for socket in "$psock" "$fsock"; do
    if sockstat -ul | grep -Fq "$socket"; then echo "socket in use: $socket" >&2; exit 1; fi
    [ ! -S "$socket" ] || unlink "$socket"
done
mkdir -p "/var/db/atrium/$person"
printf '[personas.work]\ncolour = "blue"\n' >"/var/db/atrium/$person/policy.toml"
daemon -f -o "$D/portcullis.log" env RUST_LOG=info /usr/local/bin/portcullisd --socket "$psock"
daemon -f -o "$D/fresco.log" env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG="$D/screen" FRESCOD_SOCK="$fsock" FRESCOD_WM_SOCK="$D/wm.sock" FRESCOD_INPUT_SOCK="$D/inject.sock" FRESCOD_ACTIVATION_SOCK="$D/act.sock" FRESCOD_DEV_CAPS=all "$D/frescod"
for socket in "$psock" "$fsock" "$D/inject.sock"; do
    for i in $(jot 20); do test -S "$socket" && break; sleep 1; done
    test -S "$socket"
done
chmod 666 "$psock" "$fsock"
su -m "$person" -c "portcullis policy grant $app"
su -m "$person" -c "$D/forum-dock list" | grep -F "$app"
signer=$(cat /etc/atrium/calendar-publisher)
work_store="$home/.atrium/persona-apps/work/.fasti/$signer"
personal_store="$home/.atrium/apps/.fasti/$signer"
work_jail=org_atrium_calendar__p_work
personal_jail=org_atrium_calendar
launch work
wait_ui "$work_jail" work
sleep 3
enter 7 8 22 12 10 17 44 21 8 25 12 8 26  # design review
saved "$work_store" "design review"
cp screen.png work.png
launch personal
wait_ui "$personal_jail" personal
sleep 3
enter 19 12 4 17 18 44 15 8 22 22 18 17   # piano lesson
saved "$personal_store" "piano lesson"

check_instance() {
    jail=$1
    scope=$2
    store=$3
    title=$4
    root=$(jls -j "$jail" path)
    uid=$(ps -p "$(pgrep -j "$jail" -x atrium-calendar)" -o uid= | tr -d " ")
    meta=$(awk -v id="$uid" '$1==id {print}' "$reg")
    case "$scope" in
        work) test "$meta" = "$uid $person $app $signer persona=work";;
        personal) test "$meta" = "$uid $person $app $signer";;
    esac
    server=system-fasti--$uid
    service_jid=$(jls -j "$server" jid)
    test "$service_jid" -ne "$(jls -j "$jail" jid)"
    test "$(ps -p "$(pgrep -j "$server" -x atrium-fastid)" -o uid= -o gid= | awk '{print $1, $2}')" = '65055 65055'
    test "$(jls -j "$server" ip4 ip6)" = 'disable disable'
    test "$(jls -j "$jail" vnet)" = new
    test "$(ifconfig -j "$jail" -l)" = lo0
    endpoint=/var/run/atrium/fasti/$uid
    target="$root/atrium/sockets/calendar"
    test -S "$target/calendar.sock"
    test "$(stat -f '%u %Lp' "$target/calendar.sock")" = "$uid 600"
    mount -p | awk -v src="$endpoint" -v dst="$target" '$1==src && $2==dst && $3=="nullfs" && $4 ~ /(^|,)ro(,|$)/ {ok=1} END {exit !ok}'
    test ! -e "$root/store"
    test ! -e "$root/var/run/atrium/fasti"
    mount -p | awk -v root="$root/" -v src="$store" 'index($2,root)==1 && ($1==src || index($1,"/.fasti/")!=0) {bad=1} END {exit bad}'
    srvroot=$(jls -j "$server" path)
    mount -p | awk -v src="$store" -v dst="$srvroot/store" '$1==src && $2==dst && $3=="nullfs" {ok=1} END {exit !ok}'
    test "$(stat -f '%u %Lp' "$store")" = '65055 700'
    user=$(id -nu "$uid")
    install -m 0555 installed_probe "$root/bin/installed-probe"
    today=$(date -u +%Y-%m-%d)
    jexec -u "$user" "$jail" /bin/installed-probe /atrium/sockets/calendar/calendar.sock "$today" "$title"
    echo "$uid" >"$scope.uid"
    echo "$scope UI jail $(jls -j "$jail" jid), UID $uid; Fasti jail $service_jid; verified '$title'"
}

check_instance "$work_jail" work "$work_store" "design review"
check_instance "$personal_jail" personal "$personal_store" "piano lesson"
wu=$(cat work.uid)
pu=$(cat personal.uid)
test "$wu" != "$pu"
# Give each wrong peer a deliberate, temporary route to the other socket.
# The connection must reach the real listener and receive zero bytes.
cross() {
    root=$(jls -j "$1" path)
    from=/var/run/atrium/fasti/$3
    mkdir -p "$root/test-endpoint"
    mount -t nullfs -o ro "$from" "$root/test-endpoint"
    chmod 666 "$from/calendar.sock"
    result=0
    jexec -u "$(id -nu "$2")" "$1" /bin/installed-probe /test-endpoint/calendar.sock "$(date -u +%Y-%m-%d)" --denied || result=$?
    chmod 600 "$from/calendar.sock"
    umount "$root/test-endpoint"
    return "$result"
}
cross "$work_jail" "$wu" "$pu"
cross "$personal_jail" "$pu" "$wu"

# Restart just Work's service. The same socket and open window must read
# the accepted record after a new process has reopened the store.
old=$(pgrep -j "system-fasti--$wu" -x atrium-fastid)
kill -TERM "$old"
for i in $(jot 20); do
    new=$(pgrep -j "system-fasti--$wu" -x atrium-fastid 2>/dev/null || :)
    if [ -n "$new" ] && [ "$new" != "$old" ]; then break; fi
    sleep 1
done
test -n "$new"
test "$new" != "$old"
check_instance "$work_jail" work "$work_store" "design review"
check_instance "$personal_jail" personal "$personal_store" "piano lesson"
# A different trusted publisher may sign a manifest, but has no Fasti grant.
# Close Work normally from the launcher's point of view and wait for its
# jail teardown. Fasti must continue to serve its persona during the session.
kill -TERM "$(pgrep -j "$work_jail" -x atrium-calendar)"
for i in $(jot 20); do grep -q 'exited' work.log && break; sleep 1; done
grep -q 'exited' work.log
# Capture Personal on its own. In headless mode equally sized windows
# overlap exactly, including the edge and tag of the window behind.
inj 'MOVE 26 24' 'BTN 1 1' 'BTN 1 0'
sleep 1
cp screen.png personal.png
test "$(pgrep -j "system-fasti--$wu" -x atrium-fastid)" = "$new"
bundle=/var/lib/atrium/apps/$app
openssl ecparam -name prime256v1 -genkey -noout -out other-key.pem
openssl pkey -in other-key.pem -pubout -out /etc/atrium/publishers/calendar-gate-other.pem
cp "$bundle/atrium.toml.sig" original.sig
openssl dgst -sha256 -sign other-key.pem -out "$bundle/atrium.toml.sig" "$bundle/atrium.toml"
if timeout 10 su -m "$person" -c "$D/forum-dock launch $app --persona work" >signer.log 2>&1; then exit 1; fi
grep -q 'publisher differs from the installed Fasti grant' signer.log
cp original.sig "$bundle/atrium.toml.sig"
launch work
wait_ui "$work_jail" work
check_instance "$work_jail" work "$work_store" "design review"

# Save independent opening views through the native controls. Use the
# authorized test peer to populate both schedules without editing storage.
click() {
    inj "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0'
    sleep 1
}
close_ui() {
    kill -TERM "$(pgrep -j "$1" -x atrium-calendar)"
    for i in $(jot 25); do jls -j "$1" jid >/dev/null 2>&1 || return 0; sleep 1; done
    return 1
}
today=$(date -u +%Y-%m-%d)
cal_socket=/atrium/sockets/calendar/calendar.sock
jexec -u "$(id -nu "$wu")" "$work_jail" /bin/installed-probe "$cal_socket" "$today" --work-schedule
jexec -u "$(id -nu "$pu")" "$personal_jail" /bin/installed-probe "$cal_socket" "$today" --personal-schedule

set_default() {
    click 606 60  # Opening view
    click "$1" 193
    grep -q "\"opening_view\":\"$2\"" "$3/calendar.json"
    click "$4" 60 # browse this period
    click 52 60   # temporarily return to Today
    grep -q "\"opening_view\":\"$2\"" "$3/calendar.json"
}
# Refresh after seeding so the settings write uses the latest revision.
click 80 184
set_default 174 Week "$work_store" 150
grep -q '"opening_view":"Today"' "$personal_store/calendar.json"
close_ui "$work_jail"
click 80 184
set_default 277 Month "$personal_store" 234
close_ui "$personal_jail"

# Select a date different from today using a view-specific control. A
# window that accidentally opened on Today cannot save on the expected date.
launch work
wait_ui "$work_jail" work
sleep 2
cp screen.png week-reopened.png
weekday=$(date -u +%u)
if [ "$weekday" -eq 1 ]; then row=1; else row=0; fi
target=$(date -u -v-$((weekday - 1))d -v+${row}d +%Y-%m-%d)
click 64 $((193 + row * 52))
enter 25 12 22 12 23 # visit
saved "$work_store" visit
grep -q "\"date\":\"$target\",\"title\":\"visit\"" "$work_store/calendar.json"
close_ui "$work_jail"
launch personal
wait_ui "$personal_jail" personal
sleep 2
cp screen.png month-reopened.png
first_weekday=$(date -u -v1d +%u)
if [ "$(date -u +%d)" = 01 ]; then number=2; else number=1; fi
slot=$((first_weekday + number - 2))
target=$(date -u -v${number}d +%Y-%m-%d)
click $((62 + (slot % 7) * 95)) $((192 + (slot / 7) * 60))
enter 21 12 7 8 # ride
saved "$personal_store" ride
grep -q "\"date\":\"$target\",\"title\":\"ride\"" "$personal_store/calendar.json"
grep -q '"opening_view":"Week"' "$work_store/calendar.json"
grep -q '"opening_view":"Month"' "$personal_store/calendar.json"
echo "PASS: independent opening views, temporary browsing and date selection from both reopened windows"

# Exercise the same authenticated SessionEnded message Ostiarius sends.
printf '%s\n' '{"op":"hello","version":1}' '{"op":"session_ended","user":"calfixture"}' |
    su -m _ostiarius -c "nc -N -w 20 -U $psock" >logout.log
grep -qx '{"kind":"ok"}' logout.log
for jail in "$work_jail" "$personal_jail" "system-fasti--$wu" "system-fasti--$pu"; do
    if jls -j "$jail" jid >/dev/null 2>&1; then exit 1; fi
done
mount -p | awk '/\/system-fasti--[0-9]+\// {bad=1} END {exit bad}'
saved "$work_store" "design review"
saved "$personal_store" "piano lesson"
echo "PASS: signed Forum launches, private stores, both peer refusals, restart, signer pin, reopen and logout"
