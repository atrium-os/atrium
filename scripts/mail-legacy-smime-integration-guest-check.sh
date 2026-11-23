#!/bin/sh
# Run in a private Tessera overlay with the host-staged synthetic bundle.
set -eu
test "$#" = 1
test -n "$1"
test "$(cat .isolated-gate)" = "$1"
test "$(uname -s)/$(uname -m)" = FreeBSD/arm64
test "$(mount -p | awk '$2 == "/" {print $3}')" = tessera
test "$(id -u)" = 0
d=$(pwd)
nonce=$1
while read -r digest file; do test "$(sha256 -q "$file")" = "$digest"; done < sums

run_suite() {
    bin=$1
    count=$2
    shift 2
    timeout 30 "./$bin" --list "$@" | awk '/: test$/ { sub(/: test$/, ""); print }' | sort >"$bin.expected"
    test "$(wc -l <"$bin.expected" | tr -d ' ')" = "$count"
    if ! timeout 90 "./$bin" "$@" --test-threads=1 >"$bin.out" 2>&1; then
        cat "$bin.out"
        return 1
    fi
    cat "$bin.out"
    awk '/^test .* \.\.\. ok$/ { sub(/^test /, ""); sub(/ \.\.\. ok$/, ""); print }' "$bin.out" | sort >"$bin.passed"
    diff -u "$bin.expected" "$bin.passed"
}
run_suite arcad-unit 7 server::smime
run_suite mail-smime-unit 3
run_suite mail-smime-interop 5

evidence() {
    rc=$?
    for name in frescod limend portcullis opened closed cancelled; do
        if [ -f "$d/$name.log" ]; then
            printf '\n--- %s ---\n' "$name"
            tail -34 "$d/$name.log"
        fi
    done
    exit "$rc"
}
trap evidence EXIT
ensure_user() {
    id "$1" >/dev/null 2>&1 || pw useradd "$1" -u "$2" -d /nonexistent -s /usr/sbin/nologin
    test "$(id -u "$1")" = "$2"
}
wait_socket() {
    for n in $(jot 48); do
        test ! -S "$1" || return 0
        sleep .25
    done
    echo "Missing socket $1" >&2
    return 1
}
ensure_user _arca-ui 65021
ensure_user _limen 65022
ensure_user legacymailwork 51971
ensure_user legacymailpersonal 51972
ensure_user legacyreadworker 51973
install -d -o _arca-ui -m 700 "$d/store"
install -d -m 777 "$d/sockets"

for service in atrium-portcullisd-bootstrap atrium-portcullisd-daemon atrium-jaild; do
    if service "$service" status >/dev/null 2>&1; then service "$service" stop; fi
done
port=/atrium/sockets/portcullis/portcullis.sock
for path in "$port" /var/run/atrium/jaild.sock; do
    if sockstat -ul | grep -Fq "$path"; then
        echo "Unexpected listener in the private guest at $path" >&2
        exit 1
    fi
    test ! -S "$path" || unlink "$path"
done
install -d -m 755 /usr/local/etc/atrium /var/run/atrium /atrium/sockets/portcullis
install -m 0644 devfs.rules /usr/local/etc/atrium/devfs.rules
sh -c '. /etc/rc.subr; load_rc_config devfs; devfs_rulesets="/etc/defaults/devfs.rules /etc/devfs.rules /usr/local/etc/atrium/devfs.rules"; devfs_init_rulesets'
sh jaild-policy-merge.sh jaild.policy.toml /etc/atrium/jaild.policy.toml > merged-policy.toml
install -m 0644 merged-policy.toml /etc/atrium/jaild.policy.toml
install -m 0555 atrium-jaild /usr/local/bin/atrium-jaild
service atrium-jaild onestart
wait_socket /var/run/atrium/jaild.sock

worker=org.atrium.smime-legacy-reader
install -d /etc/atrium/publishers "$d/worker/bin"
keydir=$(mktemp -d "$d/sign.XXXXXXXX")
chmod 700 "$keydir"
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$keydir/key.pem"
openssl pkey -in "$keydir/key.pem" -pubout -out /etc/atrium/publishers/mail-legacy-gate.pem
pub=$(openssl pkey -in "$keydir/key.pem" -pubout -outform DER | sha256 -q)
cat >"$d/worker/atrium.toml" <<MANIFEST
[app]
id = "$worker"
name = "Synthetic legacy S/MIME reader"
version = "1"
entry = "bin/mail-legacy-smime-worker"
[capabilities]
network = "none"
[resources]
memory = "128M"
cpu = 30
MANIFEST
install -m 0555 mail-legacy-smime-worker "$d/worker/bin/mail-legacy-smime-worker"
"$d/opifex" pack "$d/worker" --sign "$keydir/key.pem"
"$d/opifex" install "$d/worker"
rm "$keydir/key.pem"
cat >> /etc/atrium/policy.toml <<POLICY

[workers."$worker"]
callers = ["_arca-ui"]
run_as = "legacyreadworker"
concurrent = 1
POLICY
# The staged parent is a fixture with kernel peer UIDs in a separate
# launch registry; the reader worker itself is installed and jailed.
cat >"$d/registry" <<REGISTRY
51971 maillegacyfixture org.atrium.mail $pub persona=work
51972 maillegacyfixture org.atrium.mail $pub
REGISTRY
# Limen checks the signed parent's installed role grant independently.
app=/var/lib/atrium/apps/org.atrium.mail
if [ -d "$app" ]; then mv "$app" "$d/prior-mail-bundle"; fi
install -d "$app"
cat >"$app/atrium.toml" <<MANIFEST
[app]
id = "org.atrium.mail"
name = "Synthetic legacy S/MIME parent"
version = "1"
entry = "bin/fixture"
[capabilities]
embeds = ["smime-legacy-read"]
MANIFEST

s="$d/sockets"
umask 000
daemon -f -o "$d/portcullis.log" "$d/portcullisd" --socket "$port"
wait_socket "$port"
chmod 666 "$port"
daemon -f -o "$d/frescod.log" env FRESCOD_SPLIT=0 FRESCOD_DEV_CAPS=all \
    FRESCOD_HEADLESS_PNG="$d/screen" FRESCOD_SOCK="$s/fresco" FRESCOD_WM_SOCK="$s/wm" \
    FRESCOD_INPUT_SOCK="$s/input" FRESCOD_ACTIVATION_SOCK="$s/activation" FRESCOD_SLOT_UIDS=65022 \
    FRESCOD_ACTIVATION_UIDS=65021=approve FRESCOD_INJECT_ACTIVATION=1 "$d/frescod"
for socket in "$s/fresco" "$s/input" "$s/activation"; do wait_socket "$socket"; done
chmod 666 "$s/fresco" "$s/input" "$s/activation"
daemon -f -o "$d/limend.log" -u _limen env ARCA_MAIL_PUBLISHER="$pub" LIMEN_APP_REGISTRY="$d/registry" \
    LIMEN_FRESCOD_ACTIVATION="$s/activation" LIMEN_SOCKET="$s/limend" "$d/limend"
wait_socket "$s/limend"
chmod 444 "$d/.isolated-gate"
su -m _arca-ui -c "$d/arca-legacy-smime-fixture prepare $d $pub $nonce" | grep -x LEGACY_FIXTURE_STORED

scenario() {
    caller=$1
    outcome=$2
    marker=$(printf '%s' "$outcome" | tr '[:lower:]' '[:upper:]')
    daemon -f -o "$d/$outcome.log" env ARCA_APP_REGISTRY="$d/registry" LIMEN_SOCKET="$s/limend" \
        FRESCO_ACTIVATION="$s/activation" FRESCO_SOCKET="$s/fresco" \
        su -m _arca-ui -c "exec $d/arca-legacy-smime-fixture serve $d $pub $nonce"
    for n in $(jot 25); do
        if grep -qs '^LEGACY_FIXTURE_PROVIDER_READY$' "$d/$outcome.log"; then break; fi
        sleep .25
    done
    grep -qx LEGACY_FIXTURE_PROVIDER_READY "$d/$outcome.log"
    timeout 48 su -m "$caller" -c "$d/legacy_smime_sheet_probe $s/fresco $s/limend $s/input $outcome" |
        grep -x "LEGACY_PARENT_$marker"
    for n in $(jot 12); do
        if grep -qx "LEGACY_FIXTURE_$marker" "$d/$outcome.log"; then break; fi
        sleep .25
    done
    grep -qx "LEGACY_FIXTURE_$marker" "$d/$outcome.log"
    sleep 1
}
scenario legacymailwork cancelled
! grep -q LEGACY_JAILED_WORKER_READY "$d/cancelled.log"
scenario legacymailwork opened
grep -qx LEGACY_JAILED_WORKER_READY "$d/opened.log"
scenario legacymailpersonal closed
grep -qx LEGACY_JAILED_WORKER_READY "$d/closed.log"
grep -Fq "starts $worker as legacyreadworker (system worker rule)" "$d/portcullis.log"
echo MAIL_LEGACY_SMIME_INTEGRATION_TESSERA_OK
