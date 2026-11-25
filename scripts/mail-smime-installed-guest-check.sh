#!/bin/sh
# Run only in a private, staged Tessera overlay. The host supplies the
# disposable run nonce in .isolated-gate.
set -eu
test "$#" = 1
test -n "$1"
test "$(cat .isolated-gate)" = "$1"
test "$(uname -s)/$(uname -m)" = FreeBSD/arm64
test "$(mount -p | awk '$2 == "/" {print $3}')" = tessera
test "$(id -u)" = 0
d=$(pwd)
while read -r checksum file; do
    test "$(sha256 -q "$file")" = "$checksum"
done < sums

app=org.atrium.mail
person=mailcertfixture
registry=/var/run/atrium/app-registry
s=/atrium/sockets
arca=$s/arca/arcad.sock
activation=$s/fresco-activation/fresco-activation.sock
fresco=$s/fresco/fresco.sock
limen=$s/limen/limend.sock
portcullis=$s/portcullis/portcullis.sock
bootstrap=$d/atrium-portcullisd-bootstrap
wait_socket() {
    for n in $(jot 50); do
        test ! -S "$1" || return 0
        sleep .25
    done
    echo "Missing socket $1" >&2
    return 1
}
start() {
    name=$1
    shift
    daemon -f -o "$d/$name.log" "$@"
}
evidence() {
    code=$?
    for log in jaild bootstrap portcullisd frescod limend arcad arca-ui scrinium work personal; do
        if test -s "$d/$log.log"; then
            printf '\n--- %s ---\n' "$log"
            tail -35 "$d/$log.log"
        fi
    done
    exit "$code"
}
trap evidence EXIT
ensure_user() {
    id "$1" >/dev/null 2>&1 || pw useradd "$1" -u "$2" -d /nonexistent -s /usr/sbin/nologin
    test "$(id -u "$1")" = "$2"
}
ensure_user _arca 65020
ensure_user _arca-ui 65021
ensure_user _limen 65022
ensure_user _ostiarius 50093
ensure_user mailcertpicker 51973
if ! id "$person" >/dev/null 2>&1; then
    test ! -e "/home/$person"
    pw useradd "$person" -m -d "/home/$person" -s /bin/sh
fi
install -d -o "$person" -m 755 "/var/db/atrium/$person"
printf '[personas.work]\ncolour = "blue"\n' >"/var/db/atrium/$person/policy.toml"
chown "$person" "/var/db/atrium/$person/policy.toml"

# Do not collide with services that may have started with the base guest.
for service in atrium-portcullisd-bootstrap atrium-portcullisd-daemon atrium-jaild; do
    if service "$service" status >/dev/null 2>&1; then service "$service" stop; fi
done
for endpoint in "$arca" "$activation" "$fresco" "$limen" "$portcullis" /var/run/atrium/jaild.sock; do
    if sockstat -ul | grep -Fq "$endpoint"; then
        echo "Unexpected listener in the private guest at $endpoint" >&2
        exit 1
    fi
    test ! -S "$endpoint" || unlink "$endpoint"
done
# Mail's notify and arca-courier capabilities mount these socket directories.
install -d /var/run/atrium "$s/arca" "$s/sigillum" "$s/portcullis" "$s/fresco" "$s/fresco-activation" "$s/limen" \
    "$s/arca-courier" "$s/notify"
# frescod needs system-sans. A bare guest disk has no fonts, so the staging
# side copies the repo's fonts/ibm-plex and fonts/phosphor into $d/fonts.
if test -d "$d/fonts"; then
    install -d /usr/local/share/fonts/ibm-plex
    install -m 0444 "$d"/fonts/*.ttf /usr/local/share/fonts/ibm-plex/
fi
ls /usr/local/share/fonts/ibm-plex/IBMPlexSans-*.ttf >/dev/null
touch "$registry" /var/run/atrium/courier-registry
# Only Scrinium's trusted provider is injected into the registry. Portcullis
# writes the Work and Personal Mail entries when it launches the signed app.
picker_registry="51973 $person org.atrium.scrinium"
grep -qxF "$picker_registry" "$registry" || printf '%s\n' "$picker_registry" >>"$registry"
chown _arca:_arca "$s/arca"
chown _limen:_limen "$s/limen"
install -d -o _arca -g _arca -m 700 /var/db/arca
root=/var/lib/atrium/jails/arcad
for path in dev libexec lib usr/lib usr/local/lib usr/local/bin atrium/sockets/arca atrium/sockets/sigillum var/db/arca var/run/atrium; do
    install -d "$root/$path"
done
touch "$root/var/run/atrium/app-registry" "$root/var/run/atrium/courier-registry"
install -m 0555 "$d/arcad" /usr/local/bin/atrium-arcad
install -m 0644 "$d/devfs.rules" /usr/local/etc/atrium/devfs.rules
sh -c '. /etc/rc.subr; load_rc_config devfs; devfs_rulesets="/etc/defaults/devfs.rules /etc/devfs.rules /usr/local/etc/atrium/devfs.rules"; devfs_init_rulesets'
test -n "$(devfs rule -s 32 show)"
sh "$d/jaild-policy-merge.sh" "$d/jaild.policy.toml" /etc/atrium/jaild.policy.toml >"$d/policy.toml"
install -m 0644 "$d/policy.toml" /etc/atrium/jaild.policy.toml
install -m 0555 "$d/atrium-jaild" /usr/local/bin/atrium-jaild
service atrium-jaild onestart
wait_socket /var/run/atrium/jaild.sock

signing=$(mktemp -d "$d/sign.XXXXXXXX")
chmod 700 "$signing"
openssl ecparam -name prime256v1 -genkey -noout -out "$signing/key"
openssl pkey -in "$signing/key" -pubout -outform DER -out "$signing/publisher.der"
publisher=$(sha256 -q "$signing/publisher.der")
install -d /etc/atrium/publishers
openssl pkey -in "$signing/key" -pubout -out "/etc/atrium/publishers/mail-smime-gate.pem"
install -d "$d/bundle/bin"
for bin in atrium-mail mail-sync-worker mail-parse-jail mail-office-copy; do
    install -m 0555 "$d/$bin" "$d/bundle/bin/$bin"
done
install -m 0444 "$d/mail.atrium.toml" "$d/bundle/atrium.toml"
openssl dgst -sha256 -sign "$signing/key" -out "$d/bundle/atrium.toml.sig" "$d/bundle/atrium.toml"
rm "$signing/key"
if test -d "/var/lib/atrium/apps/$app"; then
    mv "/var/lib/atrium/apps/$app" "$d/prior-mail-bundle"
fi
"$d/opifex" install "$d/bundle"
cmp "$d/atrium-mail" "/var/lib/atrium/apps/$app/bin/atrium-mail"

install -d "$d/services"
sed "s/^env  = \[/env  = [{ key = \"ARCA_MAIL_PUBLISHER\", value = \"$publisher\" }, /" \
    "$d/42-atrium-arcad.toml" >"$d/services/42-atrium-arcad.toml"
start bootstrap "$bootstrap" --services-dir "$d/services" --volumes-socket ""
wait_socket "$arca"
arca_pid=$(pgrep -x atrium-arcad)
arca_jid=$(ps -o jid= -p "$arca_pid" | tr -d ' ')
test "$arca_jid" -gt 0
test "$(ps -o uid= -p "$arca_pid" | tr -d ' ')" = 65020
test "$(jls -j "$arca_jid" allow.mlock)" = true
test "$(jls -j "$arca_jid" ip4 ip6)" = 'disable disable'
procstat rlimit "$arca_pid" | grep memorylocked
printf 'MAIL_ARCA_JAILED_MLOCK_OK jid=%s\n' "$arca_jid"
su -m _ostiarius -c "$d/arca-probe $arca unlock $person local-synthetic-fixture" | grep -i '^unlocked'

install -d "$d/personas/work/Documents" "$d/personas/school/Documents"
install -m 0444 "$d/a-cert.der" "$d/personas/work/Documents/a-cert.der"
install -o mailcertpicker -m 0400 "$d/z-private.pk8" "$d/personas/work/Documents/z-private.pk8"
install -m 0444 "$d/a-cert.der" "$d/personas/school/Documents/school.der"
start frescod env FRESCOD_SPLIT=0 FRESCOD_DEV_CAPS=all FRESCOD_HEADLESS_PNG="$d/screen" \
    FRESCOD_HEADLESS_OUTPUTS=1280x840 FRESCOD_SOCK="$fresco" FRESCOD_WM_SOCK="$d/wm.sock" \
    FRESCOD_INPUT_SOCK="$d/input.sock" FRESCOD_ACTIVATION_SOCK="$activation" \
    FRESCOD_FORUM_UID=0 FRESCOD_SLOT_UIDS=65022 FRESCOD_ACTIVATION_UIDS=65021=fill+approve \
    FRESCOD_INJECT_ACTIVATION=1 "$d/frescod"
for endpoint in "$fresco" "$activation" "$d/wm.sock" "$d/input.sock"; do wait_socket "$endpoint"; done
chmod 666 "$fresco" "$activation"
start portcullisd "$d/portcullisd" --socket "$portcullis"
wait_socket "$portcullis"
chmod 666 "$portcullis"
start limend -u _limen env ARCA_MAIL_PUBLISHER="$publisher" LIMEN_FRESCOD_ACTIVATION="$activation" "$d/limend"
wait_socket "$limen"
start arca-ui -u _arca-ui env FRESCO_SOCKET="$fresco" FRESCO_ACTIVATION="$activation" "$d/arca-ui"
start scrinium -u mailcertpicker env SCRINIUM_PERSONAS="$d/personas" FRESCO_SOCKET="$fresco" "$d/scrinium" --no-window
su -m "$person" -c "$d/portcullis policy grant $app"

launch() {
    case "$1" in
        work) start work su -m "$person" -c "$d/portcullis launch $app --persona work" ;;
        personal) start personal su -m "$person" -c "$d/portcullis launch $app" ;;
        *) return 1 ;;
    esac
}
wait_mail() {
    for _ in $(jot 40); do
        if pgrep -j "$1" -x atrium-mail 2>/dev/null; then return; fi
        sleep .5
    done
    return 1
}
verify_identity() {
    pid=$1
    persona=$2
    uid=$(ps -o uid= -p "$pid" | tr -d ' ')
    actual=$(awk -v uid="$uid" '$1==uid {print}' "$registry")
    expected="$uid $person $app $publisher"
    if [ "$persona" = work ]; then expected="$expected persona=work"; fi
    test "$actual" = "$expected"
    printf '%s\n' "$uid"
}
verify_certs() {
    jail=$1
    uid=$2
    expected=$3
    root=$(jls -j "$jail" path)
    # The probe runs inside Mail's jail as its kernel UID and uses its
    # mounted Arca endpoint. Only the public test certificate is staged.
    install -m 0555 "$d/smime_installed_probe" "$root/bin/smime-certs-probe"
    install -m 0444 "$d/a-cert.der" "$root/bin/smime-fixture.der"
    jexec -u "$(id -nu "$uid")" "$jail" /bin/smime-certs-probe certs "$arca" "$expected"
}
work=org_atrium_mail__p_work
personal=org_atrium_mail
launch work
workpid=$(wait_mail "$work")
workuid=$(verify_identity "$workpid" work)
printf 'MAIL_SIGNED_WORK_UID=%s\n' "$workuid"
verify_certs "$work" "$workuid" empty | grep -x MAIL_SMIME_PUBLIC_CERTS=0
# Enumerated surfaces can precede the first composed Settings frame.
sleep 3
timeout 115 "$d/smime_installed_probe" drive "$d/wm.sock" "$d/input.sock" "$workuid"
verify_certs "$work" "$workuid" /bin/smime-fixture.der | grep -x MAIL_SMIME_PUBLIC_CERTS=1
grep -Eq 'picker [0-9]+ is for work' "$d/scrinium.log"
grep -Eq 'picker [0-9]+: handed over z-private.pk8$' "$d/scrinium.log"
cp "$d/screen.png" "$d/work.png"

launch personal
personalpid=$(wait_mail "$personal")
personaluid=$(verify_identity "$personalpid" personal)
test "$personaluid" != "$workuid"
printf 'MAIL_SIGNED_PERSONAL_UID=%s\n' "$personaluid"
verify_certs "$personal" "$personaluid" empty | grep -x MAIL_SMIME_PUBLIC_CERTS=0
echo MAIL_SIGNED_SMIME_INSTALLED_TESSERA_OK
