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
close_ui() {
    case "$1" in
        "$work_jail") scope=work ;;
        "$personal_jail") scope=personal ;;
        *) return 1 ;;
    esac
    # Portcullis holds this advisory lock across window launch and jail teardown.
    # Check it is held, then wait for release on the same inode before relaunch.
    lease="/var/run/atrium/fasti/window-$scope"
    test -f "$lease"
    if lockf -kns -t 0 "$lease" /usr/bin/true; then return 1; fi
    kill -TERM "$(pgrep -j "$1" -x atrium-calendar)"
    lockf -kns -t 25 "$lease" /usr/bin/true
    ! jls -j "$1" jid >/dev/null 2>&1
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

restart_work() {
    old=$(pgrep -j "system-fasti--$wu" -x atrium-fastid)
    kill -TERM "$old"
    for i in $(jot 20); do
        new=$(pgrep -j "system-fasti--$wu" -x atrium-fastid 2>/dev/null || :)
        if [ -n "$new" ] && [ "$new" != "$old" ]; then break; fi
        sleep 1
    done
    test -n "$new"
    test "$new" != "$old"
}

calendar_paths() {
    signer=$(cat /etc/atrium/calendar-publisher)
    work_store="$home/.atrium/persona-apps/work/.fasti/$signer"
    personal_store="$home/.atrium/apps/.fasti/$signer"
    work_jail=org_atrium_calendar__p_work
    personal_jail=org_atrium_calendar
}

mail_jail=org_atrium_mail__p_work
mail_folder="$home/.atrium/persona-apps/work/org.atrium.mail/nonexistent/.local/share/atrium-mail"
start_mail() {
    launcher_dir=$(mktemp -d "$D/mail-launch.XXXXXX")
    printf '%s\n' "$launcher_dir" >"$D/current-mail-launch"
    daemon -f -P "$launcher_dir/supervisor.pid" -p "$launcher_dir/child.pid" -o "$D/mail-work.log" su -m "$person" -c "$D/forum-dock launch org.atrium.mail --persona work"
    for i in $(jot 30); do pgrep -j "$mail_jail" -x atrium-mail >/dev/null 2>&1 && break; sleep 1; done
    pgrep -j "$mail_jail" -x atrium-mail >/dev/null
}
stop_mail() {
    launcher_dir=$(cat "$D/current-mail-launch")
    launcher=$(cat "$launcher_dir/supervisor.pid" 2>/dev/null || :)
    if pid=$(pgrep -j "$mail_jail" -x atrium-mail 2>/dev/null); then kill -TERM "$pid"; fi
    # Forum returns after Portcullis has finished unmounting the previous
    # launch. The jail disappears earlier, while those mounts can still race.
    for i in $(jot 25); do
        if ! jls -j "$mail_jail" jid >/dev/null 2>&1; then
            if [ -z "$launcher" ] || ! kill -0 "$launcher" 2>/dev/null; then return 0; fi
        fi
        sleep 1
    done
    return 1
}

# The keyboard driver observes compositor frames and sends only HID keys.
# These commands handle the trusted lifecycle on the marked disposable guest.
case "${2-}" in
    keyboard-open|keyboard-close|keyboard-store|keyboard-fingerprint|keyboard-restart|keyboard-review)
        calendar_paths
        case "${3-}" in
            work) jail=$work_jail; store=$work_store ;;
            personal) jail=$personal_jail; store=$personal_store ;;
            *) exit 2 ;;
        esac
        case "$2" in
            keyboard-open)
                launch "$3"
                wait_ui "$jail" "$3"
                uid=$(ps -p "$(pgrep -j "$jail" -x atrium-calendar)" -o uid= | tr -d ' ')
                echo "$uid" >"$3.uid"
                test "$(ifconfig -j "$jail" -l)" = lo0
                test "$(jls -j "system-fasti--$uid" ip4 ip6)" = 'disable disable'
                install -m 0555 installed_probe "$(jls -j "$jail" path)/bin/installed-probe"
                ;;
            keyboard-close) close_ui "$jail" ;;
            keyboard-fingerprint)
                if [ -f "$store/calendar.json" ]; then sha256 -q "$store/calendar.json"; else echo empty; fi ;;
            keyboard-store) cat "$store/calendar.json" ;;
            keyboard-restart) test "$3" = work; wu=$(cat work.uid); restart_work ;;
            keyboard-review)
                case "$5" in --original-review|--moved-review) ;; *) exit 2 ;; esac
                jexec -u "$(id -nu "$(cat "$3.uid")")" "$jail" /bin/installed-probe /atrium/sockets/calendar/calendar.sock "$4" "$5"
                ;;
        esac
        exit 0 ;;
    background-personal|background-arrive|background-receipt|background-check)
        calendar_paths
        case "$2" in
            background-personal)
                jexec -u "$(id -nu "$(cat personal.uid)")" "$personal_jail" /bin/installed-probe /atrium/sockets/calendar/calendar.sock 2026-12-01 --accept-personal-fixture
                saved "$personal_store" "Studio planning" ;;
            background-arrive)
                stop_mail
                uid=$(stat -f %u "$mail_folder/index.redb")
                ./invitation_probe background-batch "$mail_folder" "$D/mail-parse-jail"
                chown -R "$uid:$uid" "$mail_folder"
                start_mail ;;
            background-receipt)
                for i in $(jot 90); do
                    if grep -qs '"sequence":5' "$work_store/meeting-changes.json"; then break; fi
                    sleep 1
                done
                grep -q '"sequence":5' "$work_store/meeting-changes.json"
                if jls -j "$work_jail" jid >/dev/null 2>&1; then echo "Calendar window launched on delivery" >&2; exit 1; fi
                test ! -f "$personal_store/meeting-changes.json" ;;
            background-check)
                # This is the final store inspection. The read/write probe takes the
                # database lock itself. No later launch reuses these mounts.
                kill -TERM "$(pgrep -j "$mail_jail" -x atrium-mail)"
                for i in $(jot 20); do
                    if ! jls -j "$mail_jail" jid >/dev/null 2>&1; then break; fi
                    sleep 1
                done
                if jls -j "$mail_jail" jid >/dev/null 2>&1; then exit 1; fi
                ./invitation_probe background-check "$mail_folder" "$D/mail-parse-jail"
                test ! -f "$personal_store/meeting-changes.json" ;;
        esac
        exit 0 ;;
    invitation-check-empty|series-check-empty)
        stop_mail
        ./invitation_probe empty "$mail_folder" "$D/mail-parse-jail"
        if [ "$2" = invitation-check-empty ]; then start_mail; fi
        exit 0 ;;
    invitation-submit|series-submit)
        stop_mail
        mode=submit; if [ "$2" = series-submit ]; then mode=submit-series; fi
        ./invitation_probe "$mode" "$mail_folder" "$D/mail-parse-jail"
        if [ "$2" = series-submit ]; then start_mail; fi
        exit 0 ;;
    keyboard-setup|import-setup|invitation-setup|series-setup|bounded-setup|cancel-dates-setup|exdates-setup|future-setup|cancel-future-setup|reinstate-setup|restart-setup|restore-one-setup|reissue-setup|long-range-setup|'') ;;
    *) exit 2 ;;
esac

test ! -e "$home"
tzsetup -s UTC
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
activation="$D/act.sock"
case "${2-}" in invitation-setup|series-setup|bounded-setup|cancel-dates-setup|exdates-setup|future-setup|cancel-future-setup|reinstate-setup|restart-setup|restore-one-setup|reissue-setup|long-range-setup) mail_setup=yes ;; *) mail_setup=no ;; esac
if [ "${2-}" = import-setup ] || [ "$mail_setup" = yes ]; then
    activation=/atrium/sockets/fresco-activation/fresco-activation.sock
    if sockstat -ul | grep -Fq "$activation"; then echo "activation socket in use" >&2; exit 1; fi
fi
if [ "$mail_setup" = yes ]; then export FRESCOD_INJECT_ACTIVATION=1; fi
daemon -f -o "$D/fresco.log" env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG="$D/screen" FRESCOD_SOCK="$fsock" FRESCOD_WM_SOCK="$D/wm.sock" FRESCOD_INPUT_SOCK="$D/inject.sock" FRESCOD_ACTIVATION_SOCK="$activation" FRESCOD_DEV_CAPS=all "$D/frescod"
for socket in "$psock" "$fsock" "$D/inject.sock"; do
    for i in $(jot 20); do test -S "$socket" && break; sleep 1; done
    test -S "$socket"
done
chmod 666 "$psock" "$fsock"
su -m "$person" -c "portcullis policy grant $app"
su -m "$person" -c "$D/forum-dock list" | grep -F "$app"
if [ "$mail_setup" = yes ]; then
    if [ -d /var/lib/atrium/apps/org.atrium.mail ]; then
        test -z "$(jls -n name | grep org_atrium_mail || :)"
        mv /var/lib/atrium/apps/org.atrium.mail "$D/stock-mail"
    fi
    mkdir -p mail-app/bin
    install -m 0555 atrium-mail mail-parse-jail mail-app/bin/
    cat >mail-app/atrium.toml <<'MANIFEST'
[app]
id = "org.atrium.mail"
name = "Mail"
version = "0.1.0"
entry = "bin/atrium-mail"
[capabilities]
graphics = "fresco"
network = "none"
open-attachments = true
calendar-updates = true
net-fetch = true
[[workers]]
name = "parse"
entry = "bin/mail-parse-jail"
[[links]]
schemes = ["mailto"]
[supervision]
instances = "single"
MANIFEST
    opifex pack mail-app
    openssl dgst -sha256 -sign /root/of-key.pem -out mail-app/atrium.toml.sig mail-app/atrium.toml
    opifex install mail-app
    su -m "$person" -c 'portcullis policy grant org.atrium.mail'
    ./invitation_probe exercise "$D/standalone-store" "$D/mail-parse-jail"
    case "$2" in series-setup) mode=seed-series ;; bounded-setup) mode=seed-bounded ;; cancel-dates-setup) mode=seed-cancel-dates ;; reissue-setup) mode=seed-reissue ;; restore-one-setup) mode=seed-restore-one ;; restart-setup) mode=seed-restart ;; reinstate-setup) mode=seed-reinstate ;; cancel-future-setup) mode=seed-cancel-future ;; long-range-setup) mode=seed-long-range ;; future-setup) mode=seed-future ;; exdates-setup) mode=seed-exdates ;; *) mode=seed ;; esac
    ./invitation_probe "$mode" "$D/seeded" "$D/mail-parse-jail"
    start_mail
    for i in $(jot 20); do test -f "$mail_folder/index.redb" && break; sleep 1; done
    uid=$(stat -f %u "$mail_folder/index.redb")
    stop_mail
    cp -R seeded/. "$mail_folder"
    chown -R "$uid:$uid" "$mail_folder"
    start_mail
    exit 0
fi
if [ "${2-}" = import-setup ]; then
    # Refresh the stock image's broker and its registry mount after restarting jaild.
    install -m 0555 limend /usr/local/bin/atrium-limend
    service atrium-portcullisd-bootstrap restart
    mkdir -p files-app/bin /atrium/sockets/removable-media
    install -m 0555 scrinium files-app/bin/scrinium
    install -m 0444 scrinium-calendar-test.toml files-app/atrium.toml
    opifex pack files-app
    openssl dgst -sha256 -sign /root/of-key.pem -out files-app/atrium.toml.sig files-app/atrium.toml
    opifex install files-app
    su -m "$person" -c 'portcullis policy grant org.atrium.scrinium'
    daemon -f -o "$D/files.log" su -m "$person" -c "$D/forum-dock launch org.atrium.scrinium --persona personal"
    for i in $(jot 30); do grep -qs 'providing picker' files.log && break; sleep 1; done
    grep -q 'providing picker' files.log
    cp studio.ics unsupported.ics "$home/.atrium/personas/work/Documents/"
    cp studio.ics "$home/Documents/personal-only.ics"
    exit 0
fi
if [ "${2-}" = keyboard-setup ]; then exit 0; fi
calendar_paths
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
    for mode in --denied --denied-search --denied-undo; do
        jexec -u "$(id -nu "$2")" "$1" /bin/installed-probe /test-endpoint/calendar.sock "$(date -u +%Y-%m-%d)" "$mode" || result=$?
    done
    chmod 600 "$from/calendar.sock"
    umount "$root/test-endpoint"
    return "$result"
}
cross "$work_jail" "$wu" "$pu"
cross "$personal_jail" "$pu" "$wu"

# Restart just Work's service. The same socket and open window must read
# the accepted record after a new process has reopened the store.
restart_work
check_instance "$work_jail" work "$work_store" "design review"
check_instance "$personal_jail" personal "$personal_store" "piano lesson"
# A different trusted publisher may sign a manifest, but has no Fasti grant.
# Close Work normally from the launcher's point of view and wait for its
# jail teardown. Fasti must continue to serve its persona during the session.
close_ui "$work_jail"
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

# The first agenda row is the newly created 09:00 ride on a non-today date.
# The durable write precedes the window receiving its reply and showing Edit.
sleep 2
click 590 269
click 262 341
cp screen.png delete-review.png
click 81 416  # Back from the deletion review
saved "$personal_store" ride
click 262 341
click 273 416
if grep -q '"title":"ride"' "$personal_store/calendar.json"; then exit 1; fi
install -m 0555 installed_probe "$(jls -j "$personal_jail" path)/bin/installed-probe"
jexec -u "$(id -nu "$pu")" "$personal_jail" /bin/installed-probe "$cal_socket" "$today" --seed-search-personal
close_ui "$personal_jail"

open_work_week() {
    launch work
    wait_ui "$work_jail" work
    sleep 2
    install -m 0555 installed_probe "$(jls -j "$work_jail" path)/bin/installed-probe"
}
thursday=$(date -u -v-$((weekday - 1))d -v+3d +%Y-%m-%d)
check_review() {
    jexec -u "$(id -nu "$wu")" "$work_jail" /bin/installed-probe "$cal_socket" "$thursday" "$1"
}
open_work_week
check_review --original-review
click 62 349  # Thursday in Week
review_y=269
if [ "$weekday" -eq 4 ]; then review_y=307; fi
# The title takes initial focus. Tab through date and times to Save.
keys() {
    for code in "$@"; do inj "KEY $code 1 0" "KEY $code 0 0"; done
}
move_review() {
    click 452 "$review_y"   # Edit occurrence
    # Home, Right, forward Delete, digit 5: change 14:xx to 15:xx.
    keys 43 43 74 79 76 34 43 74 79 76 34
    sleep 1
    cp screen.png edit-occurrence.png
    keys 43 40
    sleep 1
    check_review --moved-review
}
move_review
cp screen.png undo-offered.png
click 150 60  # Browsing Week preserves this window's undo.
click 590 564
check_review --original-review
cp screen.png undo-restored.png
move_review
# Another authorized writer changes the same calendar while the old button is visible.
check_review --parallel-write
before_undo=$(sha256 -q "$work_store/calendar.json")
click 590 564
cp screen.png undo-expired.png
test "$(sha256 -q "$work_store/calendar.json")" = "$before_undo"
saved "$work_store" "Concurrent save"
check_review --moved-review
click 78 60  # Refresh the stale window.
echo "PASS: undo restores the original occurrence and availability, stale undo preserves both newer writes"
close_ui "$work_jail"
restart_work
open_work_week
check_review --moved-review
click 62 349
cp screen.png moved-agenda.png
click 593 "$review_y"   # Whole series from the moved occurrence
click 226 192
for key in 77 44 25 31; do inj "KEY $key 1 0" "KEY $key 0 0"; done # End, space, v2
click 85 341            # Review the proposed series title
cp screen.png series-review.png
click 77 60             # Cancel without applying a series change
check_review --moved-review
echo "PASS: native occurrence move, changed gaps, unchanged next week, restart and confirmed local deletion"

# Open identical titles in the two personas and prove which date each UI opened.
click 54 60  # Today anchors the range even when Thursday belongs to another month.
click 150 60 # Week
work_search() { jexec -u "$(id -nu "$wu")" "$work_jail" /bin/installed-probe "$cal_socket" "$today" "$1"; }
work_search --seed-search-work
search_lantern() {
    click 456 60
    keys 15 4 17 23 8 21 17 40   # lantern, Return in the title field
    sleep 1
}
click 456 60
keys 7 8 22 14 40  # desk, Return
sleep 1
click 197 520      # Next
cp screen.png search-page-two.png
click 72 520       # Previous
click 81 60        # Back to calendar
search_lantern
cp screen.png work-search.png
click 613 238
cp screen.png selected-search-hit.png
enter 23 21 4 6 8  # trace
saved "$work_store" trace
target=$(date -u -v2d +%Y-%m-%d)
grep -Fq "\"date\":\"$target\",\"title\":\"trace\"" "$work_store/calendar.json"
sleep 1
search_lantern
work_search --cancel-search-hit
click 613 238
cp screen.png stale-search.png
# A stale hit leaves the search fields available. Repeat the search and
# select the new first occurrence, two weeks after the original series start.
click 632 145
cp screen.png refreshed-search.png
click 613 238
enter 21 8 23 21 28   # retry
saved "$work_store" retry
target=$(date -u -v15d +%Y-%m-%d)
grep -Fq "\"date\":\"$target\",\"title\":\"retry\"" "$work_store/calendar.json"
close_ui "$work_jail"
launch personal
wait_ui "$personal_jail" personal
sleep 2
install -m 0555 installed_probe "$(jls -j "$personal_jail" path)/bin/installed-probe"
jexec -u "$(id -nu "$pu")" "$personal_jail" /bin/installed-probe "$cal_socket" "$today" --check-search-personal
search_lantern
cp screen.png personal-search.png
click 613 238
enter 11 18 16 8  # home
saved "$personal_store" home
target=$(date -u -v3d +%Y-%m-%d)
grep -Fq "\"date\":\"$target\",\"title\":\"home\"" "$personal_store/calendar.json"
echo "PASS: persona-local title search, moved occurrence selection, stale result refusal and refreshed dates"

# Enter the wall time in the native form. Keep the colon already in each field.
zone_date() {
    click 88 272
    keys 74 76 76 76 76 76 76 76 76 76 76
    keys "$@"
}
zone_time() {
    click "$1" 272
    keys 74 76 76 "$2" "$3" 79 76 76 "$4" "$5"
}
eastern() {
    click 505 272
    keys 74 76 76 76 24 22 56 8 4 22 23 8 21 17  # UTC -> us/eastern
}
sleep 1
click 333 60
click 120 193
keys 17 12 10 11 23  # night
zone_date 31 39 31 35 45 39 32 45 39 37  # 2026-03-08
zone_time 225 39 31 30 39  # 02:10
zone_time 325 39 31 33 39  # 02:40
eastern
before=$(sha256 -q "$personal_store/calendar.json")
click 87 508
test "$(sha256 -q "$personal_store/calendar.json")" = "$before"
cp screen.png zone-gap.png
zone_date 31 39 31 35 45 30 30 45 39 30  # 2026-11-01
zone_time 225 39 30 30 39
zone_time 325 39 30 33 39
click 87 508
test "$(sha256 -q "$personal_store/calendar.json")" = "$before"
click 550 321
click 550 375
cp screen.png zone-fold.png
click 87 508
saved "$personal_store" night
cp screen.png zone-agenda.png
click 593 269          # Reopen the saved event, review deletion, then leave it in place.
cp screen.png zone-reopened.png
click 260 508
cp screen.png zone-delete-review.png
click 76 60
saved "$personal_store" night

click 333 60
click 120 193
keys 22 23 24 7 12 18  # studio
zone_date 31 39 31 35 45 39 32 45 39 31  # 2026-03-02
zone_time 225 39 38 39 39
zone_time 325 30 39 39 39
eastern
click 248 508
saved "$personal_store" studio
jexec -u "$(id -nu "$pu")" "$personal_jail" /bin/installed-probe "$cal_socket" "$today" --check-zones
echo "PASS: gap and unresolved fold refused, second offset selected, weekly source time retained across DST"

# Pin Work to the current UTC zone through Settings. Change the disposable
# machine's actual time zone while both Fasti services are running.
open_work_week
click 606 60
click 405 387
grep -Fq '"pinned_zone":"UTC"' "$work_store/calendar.json"
tzsetup -s Pacific/Auckland
wait_clock() {
    file="/var/run/atrium/fasti/$1/device-zone"
    for i in $(jot 15); do
        if [ "$(cat "$file")" = "$2" ]; then return; fi
        sleep 1
    done
    return 1
}
wait_clock "$wu" Pacific/Auckland
wait_clock "$pu" Pacific/Auckland
work_search --check-pinned
close_ui "$work_jail"
# Personal remains open on March 2. Its 09:00 New York meeting now falls on March 3.
sleep 3
click 606 60
cp screen.png follows-device.png
click 82 60
jexec -u "$(id -nu "$pu")" "$personal_jail" /bin/installed-probe "$cal_socket" "$today" --check-travel
click 75 143
keys 77 42 32         # March 2 -> March 3
click 76 184
cp screen.png travel-agenda.png
enter 7 4 26 17       # dawn
saved "$personal_store" dawn
grep -Fq '"date":"2026-03-03","title":"dawn"' "$personal_store/calendar.json"
open_work_week
work_search --check-pinned
click 606 60
cp screen.png fixed-work.png
click 96 331
work_search --check-follow
echo "PASS: live device zone, midnight projection, independent Work pin, reopen and return to following"
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
