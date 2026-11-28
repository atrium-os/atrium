#!/bin/sh
# Stage the keyboard gate's binaries, praecod, reminder_probe and lib/gate.sh
# in a private, marked disposable FreeBSD desktop. This advances its clock.
set -eu
D=$(pwd)
test -n "$1"
test "$(cat .isolated-gate)" = "$1"
test "$(uname -sm)" = "FreeBSD arm64"
. "$D/lib/gate.sh"
gate_begin calendar-reminders 13
marker=$1
cal() { sh "$D/calendar-installed-guest-check.sh" "$marker" "$@"; }
click() {
    printf '%s\n' "MOVE $1 $2" 'BTN 1 1' 'BTN 1 0' | nc -w 3 -N -U inject.sock
    sleep 1
}
cal keyboard-setup
for u in calsinkd calwork calpersonal calother calunknown; do
    pw useradd "$u" -d /nonexistent -s /usr/sbin/nologin
done
sock=/atrium/sockets/notify/notify.sock
test ! -S "$sock"
install -d -m 755 -o calsinkd /atrium/sockets/notify
w=$(id -u calwork)
p=$(id -u calpersonal)
o=$(id -u calother)
x=$(id -u calunknown)
reg=$D/sink-registry
: > "$reg"
start_praeco() {
    daemon -f -u calsinkd -o "$D/praeco.log" -p "$D/praeco.pid" env PRAECO_SOCKET="$sock" PRAECO_APP_REGISTRY="$reg" PRAECO_SUBSCRIBERS="$w,$p,$o,$x" "$D/praecod"
    for i in $(jot 10); do test -S "$sock" && break; sleep 1; done
    test -S "$sock"
    chmod 666 "$sock"
    pid=$(cat praeco.pid)
    test "$(ps -p "$pid" -o comm=)" = praecod
}
restart_praeco() {
    old=$pid
    kill -TERM "$pid"
    for i in $(jot 10); do kill -0 "$old" 2>/dev/null || break; sleep 1; done
    if kill -0 "$old" 2>/dev/null; then gate_abort "old Praeco still running"; fi
    test ! -S "$sock" || unlink "$sock"
    start_praeco
    test "$old" != "$pid"
}
start_praeco
date -u 203110140800.00 >/dev/null
for persona in personal work; do cal keyboard-open "$persona"; done
work=org_atrium_calendar__p_work
personal=org_atrium_calendar
install_probe() {
    root=$(jls -j "$1" path)
    test -S "$root$sock"
    install -m 555 reminder_probe "$root/bin/reminder-probe"
}
install_probe "$work"
install_probe "$personal"
probe() { jexec -u "$(id -nu "$(cat "$1.uid")")" "$2" /bin/reminder-probe "$3" "${4-}" "${5-}"; }
registry() {
    cp /var/run/atrium/app-registry "$reg.new"
    printf '%s calfixture org.example.shelf persona=work\n' "$w" >> "$reg.new"
    printf '%s calfixture org.example.shelf\n' "$p" >> "$reg.new"
    printf '%s seconduser org.example.shelf persona=work\n' "$o" >> "$reg.new"
    mv "$reg.new" "$reg"
}
watch() { su -m "$1" -c "env PRAECO_SOCKET=$sock $D/reminder_probe watch $2 ${3:-5}"; }
ordinary() { su -m "$1" -c "env PRAECO_SOCKET=$sock $D/reminder_probe history"; }
registry
probe personal "$personal" pin
signer=$(cat /etc/atrium/calendar-publisher)
store=/home/calfixture/.atrium/persona-apps/work/.fasti/$signer
mode=$(stat -f %Lp "$store")
service_jail=system-fasti--$(cat work.uid)
service_pid=$(pgrep -j "$service_jail" -x atrium-fastid)
wait_zone() {
    for i in $(jot 20); do
        if probe work "$work" zone "$1"; then return; fi
        sleep 1
    done
    return 1
}
chmod 500 "$store"
tzsetup -s Europe/Paris
wait_zone unavailable
probe personal "$personal" zone UTC
chmod "$mode" "$store"
wait_zone Europe/Paris
test "$(pgrep -j "$service_jail" -x atrium-fastid)" = "$service_pid"
tzsetup -s UTC
wait_zone UTC
ok "same Fasti process recovered from a failed zone write while pinned Personal stayed available"
probe work "$work" seed
click 60 60
click 600 270
click 445 340
snapshot=$store/calendar.json
test "$(tr ',' '\n' < "$snapshot" | grep -c '"received":null')" = 19
click 600 270
cp screen.png opt-in.png
click 445 340
test "$(tr ',' '\n' < "$snapshot" | grep -c '"received":null')" = 20
ok "the Work editor removed and restored one opt-in at the saved start"
probe personal "$personal" review 0 0
ok "twenty Work opt-ins survive a cancellation and Personal is empty"
cal keyboard-close work
watch calwork 0 2 > catch-up.log 2>&1 & catchup=$!
for i in $(jot 5); do grep -q READY catch-up.log && break; sleep 1; done
grep -q READY catch-up.log
date -u 203110140906.02 >/dev/null
for i in $(jot 15); do
    count=$(tr ',' '\n' < "$snapshot" | grep -Ec '"received":[0-9]+' || :)
    [ "$count" = 19 ] && break
    sleep 1
done
wait "$catchup"
grep -q 'PASS received 0 live reminders' catch-up.log
ok "already-connected Work receiver saw no live catch-up on two waits"
check "window closed at wake; Fasti recorded overdue occurrences" 19 "$count"
cal keyboard-open work
install_probe "$work"
registry
probe work "$work" review 1 18
probe personal "$personal" review 0 0
ok "one private Work review groups one current and eighteen ended reminders"
for attempt in 1 2; do ordinary calwork; ordinary calunknown; done
ok "two ordinary subscriber reconnects return no Calendar history"
cal keyboard-restart work
probe work "$work" review 1 18
ok "Fasti restart retains exactly nineteen receipts"
restart_praeco
ordinary calwork
ok "Praeco restart and reconnection produce no reminder replay"
date -u 203110140907.51 >/dev/null
watch calwork 1 > matched.log 2>&1 & mw=$!
watch calpersonal 0 > personal-sink.log 2>&1 & mp=$!
watch calother 0 > other-sink.log 2>&1 & mo=$!
watch calunknown denied > unresolved.log 2>&1 & mx=$!
wait "$mx" || gate_abort "unresolved subscriber was not denied"
for f in matched.log personal-sink.log other-sink.log; do
    for i in $(jot 5); do grep -q READY "$f" && break; sleep 1; done
    grep -q READY "$f" || gate_abort "subscriber did not become ready: $f"
done
for child in "$mw" "$mp" "$mo"; do wait "$child" || gate_abort "sink assertion failed"; done
grep -q 'PASS received 1 live reminders' matched.log
ok "fresh due reminder reached the registered Work sink once"
grep -q 'PASS received 0 live reminders' personal-sink.log
grep -q 'PASS received 0 live reminders' other-sink.log
grep -q 'DENIED unresolved subscriber' unresolved.log
ok "same-owner Personal, other-owner Work and unresolved receivers excluded"
ordinary calunknown
ordinary calwork
probe work "$work" review 2 18
ok "the fresh private receipt persists and ordinary subscribers have no replay"
cal keyboard-restart work
restart_praeco
watch calwork 0 2
probe work "$work" review 2 18
probe personal "$personal" review 0 0
ok "restart after fresh delivery preserves all twenty receipts and persona isolation"
click 105 545
cp screen.png review.png
click 85 60
cp screen.png back.png
gate_end
