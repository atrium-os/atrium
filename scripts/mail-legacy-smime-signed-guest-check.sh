#!/bin/sh
# Run only in a private, staged Tessera overlay. The host supplies the
# disposable run nonce in .isolated-gate.
set -eu
umask 022
test "$#" = 1
nonce=$1
if [ "${#nonce}" -ne 48 ]; then
    echo "Expected a 48-character hexadecimal synthetic run nonce" >&2
    exit 1
fi
case "$nonce" in
    *[!a-f0-9]*) echo "Invalid synthetic run nonce" >&2; exit 1 ;;
esac
test "$(cat .isolated-gate)" = "$nonce"
test "$(uname -s)/$(uname -m)" = FreeBSD/arm64
test "$(mount -p | awk '$2 == "/" {print $3}')" = tessera
test "$(id -u)" = 0
d=$(pwd)
while read -r checksum file; do
    test "$(sha256 -q "$file")" = "$checksum"
done < sums

run_suite() {
    bin=$1
    count=$2
    shift 2
    timeout 30 "./$bin" --list "$@" | awk '/: test$/ { sub(/: test$/, ""); print }' | sort >"$bin.expected"
    test "$(wc -l <"$bin.expected" | tr -d ' ')" = "$count"
    if ! timeout 90 "./$bin" "$@" --test-threads=1 --show-output >"$bin.out" 2>&1; then
        cat "$bin.out"
        return 1
    fi
    cat "$bin.out"
    awk '/^test .* \.\.\. ok$/ { sub(/^test /, ""); sub(/ \.\.\. ok$/, ""); print }' "$bin.out" | sort >"$bin.passed"
    diff -u "$bin.expected" "$bin.passed"
}
run_suite arcad-unit 25 server::smime
run_suite mail-smime-unit 3
run_suite mail-smime-interop 5
run_suite mail-received-unit 1 legacy_received_envelope
run_suite mail-preview-unit 2 legacy_preview
run_suite limen-binding-test 1 legacy_sheet_accepts_one_selection_before_attach

app=org.atrium.mail
person=maillegacyfixture
worker=org.atrium.smime-legacy-reader
jmap_worker=org.atrium.jmap-proof-fixture
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
    for log in jaild bootstrap portcullisd frescod limend cancelled opened closed work personal; do
        if test -s "$d/$log.log"; then
            printf '\n--- %s ---\n' "$log"
            tail -35 "$d/$log.log"
        fi
    done
    # A timeout wrapper can wait on long-lived guest daemons after this
    # script exits. Report the gate's own status before that wait.
    printf 'MAIL_LEGACY_GATE_EXIT=%s\n' "$code"
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
ensure_user legacyreadworker 51973
ensure_user legacyjmapworker 51974
if id "$person" >/dev/null 2>&1; then
    echo "Expected a fresh synthetic account" >&2
    exit 1
fi
test ! -e "/home/$person"
test ! -e "/var/db/arca/users/$person"
test ! -e "/var/db/atrium/$person"
pw useradd "$person" -m -d "/home/$person" -s /bin/sh
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
install -d /var/run/atrium "$s/arca" "$s/sigillum" "$s/portcullis" "$s/fresco" "$s/fresco-activation" "$s/limen"
touch "$registry" /var/run/atrium/courier-registry
# No synthetic launch registry entries are written by the gate.
if awk -v person="$person" -v app="$app" '$2 == person && $3 == app { found=1 } END { exit !found }' "$registry"; then
    echo "Unexpected existing synthetic Mail entry in the Portcullis registry" >&2
    exit 1
fi
chown _arca:_arca "$s/arca"
chown _limen:_limen "$s/limen"
install -d -o _arca -g _arca -m 700 /var/db/arca
root=/var/lib/atrium/jails/arcad
for path in dev libexec lib usr/lib usr/local/lib usr/local/bin atrium/sockets/arca atrium/sockets/sigillum atrium/sockets/portcullis var/db/arca var/run/atrium; do
    install -d "$root/$path"
done
touch "$root/var/run/atrium/app-registry" "$root/var/run/atrium/courier-registry"
install -m 0555 "$d/arcad" /usr/local/bin/atrium-arcad
install -m 0644 "$d/devfs.rules" /usr/local/etc/atrium/devfs.rules
sh -c '. /etc/rc.subr; load_rc_config devfs; devfs_rulesets="/etc/defaults/devfs.rules /etc/devfs.rules /usr/local/etc/atrium/devfs.rules"; devfs_init_rulesets'
test -n "$(devfs rule -s 32 show)"
# Admit the gate-only environment key in this disposable jaild policy.
sed '/^    "ARCA_MAIL_PUBLISHER",$/a\
    "ARCA_LEGACY_SMIME_SYNTHETIC_USER",
' "$d/jaild.policy.toml" >"$d/synthetic-policy.toml"
grep -qx '    "ARCA_LEGACY_SMIME_SYNTHETIC_USER",' "$d/synthetic-policy.toml"
sh "$d/jaild-policy-merge.sh" "$d/synthetic-policy.toml" /etc/atrium/jaild.policy.toml >"$d/policy.toml"
install -m 0644 "$d/policy.toml" /etc/atrium/jaild.policy.toml
install -m 0555 "$d/atrium-jaild" /usr/local/bin/atrium-jaild
service atrium-jaild onestart
wait_socket /var/run/atrium/jaild.sock

signing=$(mktemp -d "$d/sign.XXXXXXXX")
chmod 700 "$signing"
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$signing/key"
openssl pkey -in "$signing/key" -pubout -outform DER -out "$signing/publisher.der"
publisher=$(sha256 -q "$signing/publisher.der")
install -d /etc/atrium/publishers
openssl pkey -in "$signing/key" -pubout -out "/etc/atrium/publishers/mail-smime-gate.pem"
install -d "$d/bundle/bin"
for bin in atrium-mail mail-sync-worker mail-parse-jail mail-office-copy smime_installed_probe arca-jmap-proof; do
    install -m 0555 "$d/$bin" "$d/bundle/bin/$bin"
done
sed '/^embeds = /s/"smime-provision"/"smime-provision", "smime-legacy-read"/' \
    "$d/mail.atrium.toml" >"$d/bundle/atrium.toml"
printf 'atrium-mail-legacy-synthetic-v1\n' >"$d/bundle/bin/mail-legacy-synthetic-gate"
"$d/opifex" pack "$d/bundle" --sign "$signing/key"
if test -d "/var/lib/atrium/apps/$app"; then
    mv "/var/lib/atrium/apps/$app" "$d/prior-mail-bundle"
fi
"$d/opifex" install "$d/bundle"
cmp "$d/atrium-mail" "/var/lib/atrium/apps/$app/bin/atrium-mail"
install -d "$d/worker/bin"
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
install -m 0555 "$d/mail-legacy-smime-worker" "$d/worker/bin/mail-legacy-smime-worker"
"$d/opifex" pack "$d/worker" --sign "$signing/key"
"$d/opifex" install "$d/worker"

install -d "$d/jmap-worker/bin" "$d/jmap-worker/fixture"
cat >"$d/jmap-worker/atrium.toml" <<MANIFEST
[app]
id = "$jmap_worker"
name = "Synthetic JMAP account proof"
version = "1"
entry = "bin/arca-jmap-proof"
[capabilities]
network = "loopback"
[resources]
memory = "128M"
cpu = 30
MANIFEST
install -m 0555 "$d/arca-jmap-proof" "$d/jmap-worker/bin/arca-jmap-proof"
install -m 0444 "$d/jmap-ca.der" "$d/jmap-worker/fixture/ca.der"
install -m 0444 "$d/jmap-server.der" "$d/jmap-worker/fixture/server.der"
# Disposable fixture leaf key, usable only with the private synthetic CA.
install -m 0444 "$d/jmap-key.pk8" "$d/jmap-worker/fixture/key.pk8"
"$d/opifex" pack "$d/jmap-worker" --sign "$signing/key"
"$d/opifex" install "$d/jmap-worker"
cmp "$d/arca-jmap-proof" "/var/lib/atrium/apps/$jmap_worker/bin/arca-jmap-proof"
"$d/portcullis" policy grant --system "$jmap_worker"
rm "$signing/key"
cat >> /etc/atrium/policy.toml <<POLICY

[workers."$worker"]
callers = ["_arca"]
run_as = "legacyreadworker"
concurrent = 1

[workers."$jmap_worker"]
callers = ["_arca"]
run_as = "legacyjmapworker"
concurrent = 1
POLICY

install -d "$d/services"
# Mount only the Portcullis socket directory, in this disposable daemon config.
# The stock Arca service and Mail manifest remain unchanged.
awk -v publisher="$publisher" -v user="$person" '
    /^\[exec\]/ {
        print "[[mounts]]"
        print "source = \"/atrium/sockets/portcullis\""
        print "dest = \"atrium/sockets/portcullis\""
        print "kind = \"rw_nullfs\""
        print ""
    }
    /^env  = \[/ {
        sub(/\[/, "[{ key = \"ARCA_MAIL_PUBLISHER\", value = \"" publisher "\" }, { key = \"ARCA_LEGACY_SMIME_SYNTHETIC_USER\", value = \"" user "\" }, ")
    }
    { print }
' "$d/42-atrium-arcad.toml" >"$d/services/42-atrium-arcad.toml"
start bootstrap "$bootstrap" --services-dir "$d/services" --volumes-socket ""
wait_socket "$arca"
arca_pid=$(pgrep -x atrium-arcad)
arca_jid=$(ps -o jid= -p "$arca_pid" | tr -d ' ')
test "$arca_jid" -gt 0
test "$(ps -o uid= -p "$arca_pid" | tr -d ' ')" = 65020
test "$(jls -j "$arca_jid" allow.mlock)" = true
test "$(jls -j "$arca_jid" ip4 ip6)" = 'disable disable'
procstat rlimit "$arca_pid" | grep memorylocked
printf 'MAIL_LEGACY_ARCA_JAILED_OK jid=%s\n' "$arca_jid"
su -m _ostiarius -c "$d/arca-probe $arca unlock $person local-synthetic-fixture" | grep -i '^unlocked'

start frescod env FRESCOD_SPLIT=0 FRESCOD_DEV_CAPS=all FRESCOD_HEADLESS_PNG="$d/screen" \
    FRESCOD_HEADLESS_OUTPUTS=1280x840 FRESCOD_SOCK="$fresco" FRESCOD_WM_SOCK="$d/wm.sock" \
    FRESCOD_INPUT_SOCK="$d/input.sock" FRESCOD_ACTIVATION_SOCK="$activation" \
    FRESCOD_FORUM_UID=0 FRESCOD_SLOT_UIDS=65022 FRESCOD_ACTIVATION_UIDS=65021=approve \
    FRESCOD_INJECT_ACTIVATION=1 "$d/frescod"
for endpoint in "$fresco" "$activation" "$d/wm.sock" "$d/input.sock"; do wait_socket "$endpoint"; done
chmod 666 "$fresco" "$activation"
start portcullisd "$d/portcullisd" --socket "$portcullis"
wait_socket "$portcullis"
chmod 666 "$portcullis"
start limend -u _limen env ARCA_MAIL_PUBLISHER="$publisher" LIMEN_FRESCOD_ACTIVATION="$activation" "$d/limend"
wait_socket "$limen"
chmod 444 "$d/.isolated-gate"
su -m "$person" -c "$d/portcullis policy grant $app"

wait_mail() {
    for _ in $(jot 40); do
        if pgrep -j "$1" -x atrium-mail 2>/dev/null; then return; fi
        sleep .5
    done
    return 1
}

verify_uid() {
    pid=$1
    persona=$2
    uid=$(ps -o uid= -p "$pid" | tr -d ' ')
    expected="$uid $person $app $publisher"
    if [ "$persona" = work ]; then expected="$expected persona=work"; fi
    test "$(awk -v uid="$uid" '$1==uid {print}' "$registry")" = "$expected"
    printf '%s\n' "$uid"
}

worker_count() {
    awk '/_arca starts org.atrium.smime-legacy-reader as legacyreadworker \(system worker rule\)/ {n++}
        END {print n+0}' "$d/portcullisd.log"
}

wait_marker() {
    for _ in $(jot 40); do
        if grep -qx "$2" "$1"; then return; fi
        sleep .25
    done
    echo "Missing marker $2 in $1" >&2
    return 1
}

scenario() {
    outcome=$1
    action=$2
    login=$3
    uid=$4
    workers=$5
    scenario_log=${6:-$outcome}
    marker=$(printf '%s' "$outcome" | tr '[:lower:]' '[:upper:]')
    prior_lines=$(wc -l <"$d/$login.log")
    start "$scenario_log" env FRESCO_SOCKET="$fresco" FRESCO_ACTIVATION="$activation" \
        LIMEN_SOCKET="$limen" ARCA_APP_REGISTRY="$registry" \
        su -m _arca-ui -c "exec $d/arca-legacy-smime-fixture signed $d $publisher $nonce"
    wait_marker "$d/$scenario_log.log" LEGACY_FIXTURE_PROVIDER_READY
    timeout 52 "$d/smime_installed_probe" drive-legacy "$d/wm.sock" "$d/input.sock" "$uid" "$action"
    wait_marker "$d/$scenario_log.log" "LEGACY_FIXTURE_$marker"
    case "$action" in
        enroll|forget|cancel-enroll|cancel-forget)
            ! grep -q 'LEGACY_FIXTURE_SELECTED_MESSAGE=' "$d/$scenario_log.log"
            ;;
        *)
            expected_message=1
            if [ "$action" = unsupported ]; then expected_message=2; fi
            wait_marker "$d/$scenario_log.log" "LEGACY_FIXTURE_SELECTED_MESSAGE=$expected_message"
            ;;
    esac
    for _ in $(jot 40); do
        tail -n "+$((prior_lines + 1))" "$d/$login.log" >"$d/$scenario_log.mail"
        if grep -qx "MAIL_SIGNED_LEGACY_$marker" "$d/$scenario_log.mail"; then break; fi
        sleep .25
    done
    grep -qx "MAIL_SIGNED_LEGACY_$marker" "$d/$scenario_log.mail"
    grep -qx "MAIL_SIGNED_LEGACY_${marker}_UNREAD_NO_WRITES" "$d/$scenario_log.mail"
    echo "MAIL_SIGNED_LEGACY_${marker}_UNREAD_NO_WRITES"
    test "$(worker_count)" = "$workers"
    jls -q name >"$d/$scenario_log.jails"
    if grep -F 'app-org-atrium-smime-legacy-reader' "$d/$scenario_log.jails"; then
        echo "Legacy reader jail remained after $outcome" >&2
        exit 1
    fi
    echo "MAIL_SIGNED_LEGACY_$marker"
    case "$outcome" in
        opened|closed) test "$(grep -xc 'LEGACY_JAILED_WORKER_EXIT=0' "$d/$scenario_log.log")" = 1 ;;
        *) ! grep -q 'LEGACY_JAILED_WORKER_EXIT=' "$d/$scenario_log.log" ;;
    esac
    # The trusted fixture keeps its connection briefly to deliver the outcome.
    sleep 1
}

work=org_atrium_mail__p_work
personal=org_atrium_mail
start work su -m "$person" -c "$d/portcullis launch $app --persona work"
workuid=$(verify_uid "$(wait_mail "$work")" work)
echo "MAIL_SIGNED_WORK_UID=$workuid"
jexec -u "$(id -nu "$workuid")" "$work" /bin/smime_installed_probe legacy-denied "$arca" "$workuid"
test "$(worker_count)" = 0
scenario cancelled cancel work "$workuid" 0
scenario cancelled cancel-enroll work "$workuid" 0 cancel-enroll
scenario closed approve work "$workuid" 1 before-enroll
scenario enrolled enroll work "$workuid" 1 enroll
scenario opened approve work "$workuid" 2 after-enroll
scenario cancelled cancel-forget work "$workuid" 2 cancel-forget
scenario opened approve work "$workuid" 3 after-cancel-forget
scenario forgotten forget work "$workuid" 3 forget
scenario closed approve work "$workuid" 4 after-forget
scenario enrolled enroll work "$workuid" 4 re-enroll
scenario opened approve work "$workuid" 5 after-re-enroll
echo MAIL_LEGACY_ENROLLMENT_LIFECYCLE_OK

start personal su -m "$person" -c "$d/portcullis launch $app"
personaluid=$(verify_uid "$(wait_mail "$personal")" personal)
test "$personaluid" != "$workuid"
echo "MAIL_SIGNED_PERSONAL_UID=$personaluid"
scenario closed approve personal "$personaluid" 6

worker_total=6
for sample in 1 2 3; do
    for profile in missing-key unsupported; do
        worker_total=$((worker_total + 1))
        scenario closed "$profile" personal "$personaluid" "$worker_total" "$profile-$sample"
    done
done

awk '$2 == "ui" && $3 == "op=0x0067" && $5 == "status=1" { abandoned++ }
$2 == "ui" && $3 == "op=0x006f" {
    if ($5 == "status=0") opened++
    else if ($5 == "status=1") closed++
    else unexpected++
}
$2 == "ui" && $3 == "op=0x00e7" {
    if ($5 == "status=0") managed++
    else if ($5 == "status=1") cancelled++
    else unexpected++
} END { exit !(abandoned == 1 && opened == 3 && closed == 9 && managed == 3 && cancelled == 2 && !unexpected) }' /var/db/arca/audit.log
echo MAIL_LEGACY_ARCA_DISPATCH_OK

# Synthetic JMAP account: Arca seals the approved account and its receipt
# key, keeps both across an arcad crash, and revocation survives a restart.
# The gate runs last because it restarts arcad.
fences=/var/db/arca/legacy-jmap-fences
test ! -e "$fences"
jexec -u "$(id -nu "$workuid")" "$work" /bin/arca-jmap-proof deny "$arca" "$workuid"
su -m _arca-ui -c "exec $d/arca-jmap-proof deny $arca $personaluid"
jmap_probe() {
    su -m _arca-ui -c "exec $d/arca-jmap-proof probe $arca $workuid $1" >"$d/jmap-$2.log"
    cat "$d/jmap-$2.log"
    grep -qx "MAIL_JMAP_PROOF_ACCOUNT_$(printf '%s' "$1" | tr '[:lower:]' '[:upper:]')" "$d/jmap-$2.log"
}
authority() {
    sed -n 's/^MAIL_JMAP_PROOF_AUTHORITY=\([0-9a-f]\{16\}\)$/\1/p' "$d/jmap-$1.log"
}
restart_arca() {
    old=$(pgrep -x atrium-arcad)
    # A crash, not a clean stop: the supervisor restarts arcad on failure.
    kill -KILL "$old"
    new=
    for _ in $(jot 60); do
        new=$(pgrep -x atrium-arcad || true)
        if [ -n "$new" ] && [ "$new" != "$old" ]; then break; fi
        sleep .5
    done
    test -n "$new" && test "$new" != "$old"
    for _ in $(jot 40); do
        if su -m _ostiarius -c "$d/arca-probe $arca unlock $person local-synthetic-fixture" 2>/dev/null |
            grep -qi '^unlocked'; then
            printf 'MAIL_JMAP_PROOF_ARCA_RESTARTED pid=%s\n' "$new"
            return
        fi
        sleep .5
    done
    echo "arcad did not come back unlocked" >&2
    return 1
}
jmap_probe new first
first=$(authority first)
test -n "$first"
test "$(stat -f %Sp "$fences")" = drwx------
restart_arca
jmap_probe kept restarted
test "$(authority restarted)" = "$first"
su -m _arca-ui -c "exec $d/arca-jmap-proof revoke $arca $workuid 1"
su -m _arca-ui -c "exec $d/arca-jmap-proof revoke $arca $workuid 0"
test "$(find "$fences" -name 'account-*' -perm 600 | wc -l | tr -d ' ')" = 1
restart_arca
su -m _arca-ui -c "exec $d/arca-jmap-proof revoke $arca $workuid 0"
jmap_probe new re-enrolled
again=$(authority re-enrolled)
test -n "$again" && test "$again" != "$first"
jmap_probe kept re-enrolled-kept
test "$(authority re-enrolled-kept)" = "$again"
test "$(grep -Fc "_arca starts $jmap_worker as legacyjmapworker (system worker rule)" "$d/portcullisd.log")" = 12
jls -q name >"$d/jmap-proof.jails"
! grep -F 'app-org-atrium-jmap-proof-fixture' "$d/jmap-proof.jails"
echo MAIL_JMAP_PROOF_PERSISTED_REVOKED_RECOVERED_OK
echo MAIL_JMAP_PROOF_SIGNED_TESSERA_OK
if grep -Ei 'broken pipe|caller hung up; removing worker jail' "$d/portcullisd.log"; then
    echo "Portcullis detected a premature disconnect" >&2
    exit 1
fi
echo MAIL_LEGACY_WORKER_TEARDOWN_OK
echo MAIL_LEGACY_SMIME_SIGNED_TESSERA_OK
