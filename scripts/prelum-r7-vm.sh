#!/bin/sh
# Prelum R7's desktop-to-IPPS gate. Starts its own disposable FreeBSD guest.
set -eu
if [ "$#" -ne 4 ]; then
    echo "usage: $0 BASE_QCOW2 EFI_CODE EFI_VARS SSH_KEY" >&2
    exit 2
fi
scripts=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo=$(git -C "$scripts" rev-parse --show-toplevel)
base=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
code=$2
vars=$3
key=$4
qemu=${QEMU:-qemu-system-aarch64}
ssh_port=${PRELUM_R7_SSH_PORT:-6329}
ipps_port=${PRELUM_R7_IPPS_PORT:-19329}
keep_vm=${PRELUM_R7_KEEP_VM:-0}
case "$keep_vm" in
    0|1) ;;
    *) echo "PRELUM_R7_KEEP_VM must be 0 or 1" >&2; exit 2 ;;
esac
guest=/tmp/prelum-r7-desktop-gate
for path in "$base" "$code" "$vars" "$key"; do
    test -f "$path" || { echo "Missing gate input: $path" >&2; exit 2; }
done
for program in "$qemu" qemu-img ssh python3 openssl pdftotext pdfimages pdfinfo pdftoppm cargo clang; do
    command -v "$program" >/dev/null || { echo "Missing host tool: $program" >&2; exit 2; }
done
python3 - "$ssh_port" "$ipps_port" <<'PY'
import socket
import sys
ports = [int(port) for port in sys.argv[1:]]
if len(set(ports)) != 2 or not all(1024 <= port <= 65535 and port != 2222 for port in ports):
    sys.exit("Use two distinct nonprivileged ports outside the shared VM's port.")
for port in ports:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", port))
PY
D=$(mktemp -d "${TMPDIR:-/tmp}/prelum-r7.XXXXXX")
marker=$(openssl rand -hex 16)
echo "Gate artifacts: $D"
if ! git -C "$repo" diff HEAD --quiet; then
    echo "Commit tracked changes before running the gate so the revision identifies the build inputs." >&2
    exit 2
fi
git -C "$repo" rev-parse HEAD >"$D/revision"
sh "$scripts/prelum-r7-build.sh" "$repo" "$D"
test "$(git -C "$repo" rev-parse HEAD)" = "$(cat "$D/revision")"
git -C "$repo" diff HEAD --quiet
qemu_pid=
sim_pid=
guest_connected=
remote() {
    ssh -i "$key" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=4 \
        -o "UserKnownHostsFile=$D/known_hosts" -o StrictHostKeyChecking=accept-new \
        -p "$ssh_port" root@127.0.0.1 "$@"
}
collect_guest_artifacts() {
    remote "cd '$guest' && test \$(cat .isolated-gate) = '$marker' && sh -s" <<'GUEST'
set -eu
set --
for artifact in *.log *-preview.png screen.png jobs.txt mail-before-cancel.txt mail-after-cancel.txt; do
    if [ -f "$artifact" ] && [ ! -L "$artifact" ]; then set -- "$@" "$artifact"; fi
done
if [ "$#" -gt 0 ]; then tar -czf - "$@"; fi
GUEST
}
cleanup() {
    status=$?
    trap - EXIT
    evidence_collected=1
    if [ -n "$guest_connected" ]; then evidence_collected=0; fi
    if [ -n "$qemu_pid" ] && kill -0 "$qemu_pid" 2>/dev/null; then
        if [ -n "$guest_connected" ]; then
            if collect_guest_artifacts >"$D/guest-artifacts.tar.gz" 2>"$D/guest-artifacts.log"; then
                evidence_collected=1
            else
                echo "Could not collect guest artifacts; see $D/guest-artifacts.log" >&2
                status=1
            fi
            remote "test \$(cat '$guest/.isolated-gate') = '$marker' && shutdown -p now" \
                >"$D/shutdown.log" 2>&1 || status=1
        fi
        for i in $(seq 30); do
            kill -0 "$qemu_pid" 2>/dev/null || break
            sleep 1
        done
        if kill -0 "$qemu_pid" 2>/dev/null; then
            echo "Isolated QEMU still running (pid $qemu_pid, logs in $D)" >&2
            status=1
        fi
    fi
    if [ -n "$qemu_pid" ] && ! kill -0 "$qemu_pid" 2>/dev/null; then
        if wait "$qemu_pid"; then
            if [ "$keep_vm" -eq 0 ] && [ "$evidence_collected" -eq 1 ]; then
                if python3 "$scripts/disposable-vm-cleanup.py" "$D" vm.qcow2; then
                    if [ ! -e "$D/vm.qcow2" ]; then rm -f "$D/vars.fd" || status=1; fi
                else
                    status=1
                fi
            fi
        else
            status=1
        fi
    fi
    if [ -n "$sim_pid" ]; then
        kill "$sim_pid" 2>/dev/null || :
        wait "$sim_pid" 2>/dev/null || :
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
python3 "$scripts/prelum-r7-fixtures.py" "$D/fixtures"
printf 'idle\n' >"$D/state"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=prelum-r7-test \
    -keyout "$D/ipps.key" -out "$D/ipps.pem" >"$D/cert.log" 2>&1
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=prelum-r7-test \
    -keyout "$D/ipps-alt.key" -out "$D/ipps-alt.pem" >"$D/alt-cert.log" 2>&1
original_key=$(openssl x509 -in "$D/ipps.pem" -pubkey -noout | openssl dgst -sha256 | awk '{print $NF}')
alternate_key=$(openssl x509 -in "$D/ipps-alt.pem" -pubkey -noout | openssl dgst -sha256 | awk '{print $NF}')
test "$original_key" != "$alternate_key"
check_served_key() {
    actual=$(openssl s_client -connect "127.0.0.1:$ipps_port" -noservername </dev/null 2>>"$D/tls-probe.log" |
        openssl x509 -pubkey -noout | openssl dgst -sha256 | awk '{print $NF}')
    printf '%s %s\n' "$1" "$actual" >>"$D/tls-key-checks.log"
    test "$actual" = "$2"
}
python3 -u "$scripts/prelum-fake-printer.py" --bind 127.0.0.1 --port "$ipps_port" \
    --state "$D/state" --spool "$D/spool" --uuid-override-file "$D/printer-uuid" --tls-cert "$D/ipps.pem" --tls-key "$D/ipps.key" \
    --test-pin-job "PIN after TLS key restoration" --test-pin-job "PIN after UUID restoration" \
    --test-pin-job "Fresh PIN after replay refusal" --test-pin-rejections \
    --test-pin-job "PIN recovery after ipp-refused" --test-pin-job "PIN recovery after http-refused" \
    --test-pin-job "PIN recovery after dropped" --test-pin-job "PIN recovery after truncated" \
    --test-no-send-document-flag "$D/no-send-document" --test-no-job-password-flag "$D/no-job-password" \
    --test-no-create-job-flag "$D/no-create-job" \
    --tls-alt-cert "$D/ipps-alt.pem" --tls-alt-key "$D/ipps-alt.key" --tls-alt-flag "$D/use-alt-key" \
    >"$D/simulator.log" 2>&1 &
sim_pid=$!
qemu-img create -q -f qcow2 -F qcow2 -b "$base" "$D/vm.qcow2"
cp "$vars" "$D/vars.fd"
"$qemu" -machine virt,gic-version=3 -accel "${PRELUM_R7_ACCEL:-hvf}" \
    -cpu "${PRELUM_R7_CPU:-host}" -smp 4 -m 6144 -display none \
    -serial "file:$D/serial.log" -monitor none \
    -drive "if=pflash,format=raw,readonly=on,file=$code" \
    -drive "if=pflash,format=raw,file=$D/vars.fd" \
    -drive "if=none,id=disk,format=qcow2,file=$D/vm.qcow2" -device virtio-blk-pci,drive=disk \
    -netdev "user,id=n,net=1.2.3.0/24,host=1.2.3.2,hostfwd=tcp:127.0.0.1:$ssh_port-:22" \
    -device virtio-net-pci,netdev=n >"$D/qemu.log" 2>&1 &
qemu_pid=$!
for i in $(seq 80); do
    kill -0 "$qemu_pid"
    if remote 'test "$(uname -s)" = FreeBSD' >"$D/ssh.log" 2>&1; then break; fi
    sleep 1
done
kill -0 "$qemu_pid"
remote "umask 022; mkdir '$guest' && printf '%s\n' '$marker' >'$guest/.isolated-gate' && mkdir '$guest/files'"
guest_connected=1
kill -0 "$sim_pid"
while read -r digest program; do
    remote "cd '$guest' && cat >'$program' && chmod 755 '$program' && test \$(sha256 -q '$program') = '$digest'" <"$D/bin/$program"
    printf '%s  %s\n' "$digest" "$program"
done <"$D/built.sha256" >"$D/staged.sha256"
cmp "$D/built.sha256" "$D/staged.sha256"
for file in quicklook.html sheets-proof.xlsx slides-proof.pptx; do
    target=$file
    [ "$file" != quicklook.html ] || target=files/$file
    remote "cat >'$guest/$target'" <"$D/fixtures/$file"
done
remote "mkdir '$guest/text-files' && cat >'$guest/text-files/quicklook-proof.txt'" \
    <"$D/fixtures/quicklook-proof.txt"
remote "mkdir '$guest/image-files' && cat >'$guest/image-files/quicklook-picture.png'" \
    <"$D/fixtures/quicklook-picture.png"
remote "mkdir '$guest/pdf-files' && cat >'$guest/pdf-files/quicklook.pdf'" <"$D/fixtures/quicklook.pdf"
remote "cat >'$guest/replacement.pdf'" <"$D/fixtures/replacement.pdf"
if ! remote "sh -s -- '$guest' '$ipps_port' '$marker'" <"$scripts/prelum-r7-guest.sh" >"$D/results.log" 2>&1; then
    remote "cd '$guest' && tail -60 app.log text-app.log image-app.log pdf-app.log limend.log portcullis.log sheet.log mail.log" >"$D/guest-errors.log" 2>&1 || :
    cat "$D/results.log" >&2
    exit 1
fi
cat "$D/results.log"
remote "cat '$guest/jobs.txt'" >"$D/desktop-jobs.txt"
# Change only the advertised printer UUID. Keep the TLS listener and key.
printf '%s\n' 12d7ac00-4bd5-4413-8da5-e6952965ffcf >"$D/printer-uuid"
remote "cat >'$guest/identity-proof.pdf'" <"$D/fixtures/quicklook.pdf"
if ! remote "sh -s -- '$guest' '$marker' hold uuid" <"$scripts/prelum-r7-identity.sh" >"$D/identity-check.log" 2>&1; then
    cat "$D/identity-check.log" >&2
    exit 1
fi
cat "$D/identity-check.log"
if ! remote "sh -s -- '$guest' '$marker' retry uuid" <"$scripts/prelum-r7-identity.sh" >"$D/identity-retry.log" 2>&1; then
    cat "$D/identity-retry.log" >&2
    exit 1
fi
cat "$D/identity-retry.log"
# The transcript and spool checks below also reject any upload from the held job.
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" "$D/desktop-jobs.txt" >"$D/pdf-checks.log"
cat "$D/pdf-checks.log"

# The held-phase checks above prove there was no creation or upload before restoration.
mv "$D/printer-uuid" "$D/held-printer-uuid.txt"
if ! remote "sh -s -- '$guest' '$marker' release uuid" <"$scripts/prelum-r7-identity.sh" >"$D/identity-release.log" 2>&1; then
    cat "$D/identity-release.log" >&2
    exit 1
fi
cat "$D/identity-release.log"
remote "cat '$guest/identity-released-jobs.log'" >"$D/released-jobs.txt"
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" "$D/released-jobs.txt" "$D/fixtures/quicklook.pdf" 12 >"$D/released-pdf-checks.log"
cat "$D/released-pdf-checks.log"


# Rotate the certificate key without changing the restored printer UUID.
touch "$D/use-alt-key"
check_served_key changed "$alternate_key"
if ! remote "sh -s -- '$guest' '$marker' hold tls" <"$scripts/prelum-r7-identity.sh" >"$D/tls-identity-check.log" 2>&1; then
    cat "$D/tls-identity-check.log" >&2
    exit 1
fi
cat "$D/tls-identity-check.log"
check_served_key retry "$alternate_key"
if ! remote "sh -s -- '$guest' '$marker' retry tls" <"$scripts/prelum-r7-identity.sh" >"$D/tls-identity-retry.log" 2>&1; then
    cat "$D/tls-identity-retry.log" >&2
    exit 1
fi
cat "$D/tls-identity-retry.log"
# Require the same twelve uploads after both holds, before restoring the key.
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" "$D/released-jobs.txt" "$D/fixtures/quicklook.pdf" 12 >"$D/tls-held-pdf-checks.log"

mv "$D/use-alt-key" "$D/held-alt-key.txt"
check_served_key restored "$original_key"
if ! remote "sh -s -- '$guest' '$marker' release tls" <"$scripts/prelum-r7-identity.sh" >"$D/tls-identity-release.log" 2>&1; then
    cat "$D/tls-identity-release.log" >&2
    exit 1
fi
cat "$D/tls-identity-release.log"
remote "cat '$guest/tls-identity-released-jobs.log'" >"$D/tls-released-jobs.txt"
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" "$D/tls-released-jobs.txt" "$D/fixtures/quicklook.pdf" 13 >"$D/tls-released-pdf-checks.log"
cat "$D/tls-released-pdf-checks.log"


# Bind a one-use synthetic PIN to the original key and UUID before rotating.
pin_spki=$(openssl x509 -in "$D/ipps.pem" -pubkey -noout |
    openssl pkey -pubin -outform DER | openssl dgst -sha256 | awk '{print $NF}')
printer_uuid=$(sed -nE 's/^fake printer: listening on [0-9]+ \(urn:uuid:([a-f0-9-]+)\)$/\1/p' "$D/simulator.log")
test "${#pin_spki}" -eq 64
test "${#printer_uuid}" -eq 36
remote "sh -s -- '$guest' '$marker' '$pin_spki' '$printer_uuid' start" \
    <"$scripts/prelum-r7-pin-stage.sh" >"$D/pin-stage.log" 2>&1

touch "$D/use-alt-key"
check_served_key pin-changed "$alternate_key"
remote "sh -s -- '$guest' '$marker' hold pin" <"$scripts/prelum-r7-identity.sh" >"$D/pin-held.log" 2>&1 || { cat "$D/pin-held.log" >&2; exit 1; }
cat "$D/pin-held.log"
check_pin_blocked() {
    audit="$D/pin-$1-audit.log"
    remote "cat '$guest/fixture-arca/audit.log'" >"$audit"
    test "$(grep -cE 'ui op=0x0068 .*status=0$' "$audit")" -eq "$2"
    if [ "$(grep -c 'op=0x0011' "$audit" || :)" -ne "$(($2 - 1))" ]; then
        echo "Arca received an unexpected PIN redemption request during the $1 check" >&2
        return 1
    fi
    python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" \
        "$3" "$D/fixtures/quicklook.pdf" "$4" >"$D/pin-$1-pdf-checks.log"
}
check_pin_blocked held 1 "$D/tls-released-jobs.txt" 13

check_served_key pin-retry "$alternate_key"
remote "sh -s -- '$guest' '$marker' retry pin" <"$scripts/prelum-r7-identity.sh" >"$D/pin-retry.log" 2>&1 || { cat "$D/pin-retry.log" >&2; exit 1; }
cat "$D/pin-retry.log"
check_pin_blocked retry 1 "$D/tls-released-jobs.txt" 13

mv "$D/use-alt-key" "$D/pin-held-alt-key.txt"
check_served_key pin-restored "$original_key"
remote "sh -s -- '$guest' '$marker' release pin" <"$scripts/prelum-r7-identity.sh" >"$D/pin-release.log" 2>&1 || { cat "$D/pin-release.log" >&2; exit 1; }
cat "$D/pin-release.log"
remote "cat '$guest/fixture-arca/audit.log'" >"$D/pin-released-audit.log"
test "$(grep -c 'op=0x0011' "$D/pin-released-audit.log")" -eq 1
grep -E 'prelum-ipp op=0x0011 .*status=0$' "$D/pin-released-audit.log"
remote "cat '$guest/pin-identity-released-jobs.log'" >"$D/pin-released-jobs.txt"
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" "$D/pin-released-jobs.txt" "$D/fixtures/quicklook.pdf" 14 >"$D/pin-released-pdf-checks.log"
cat "$D/pin-released-pdf-checks.log"


# Stage a fresh PIN in the same Arca store, then change only the printer UUID.
remote "sh -s -- '$guest' '$marker' '$pin_spki' '$printer_uuid' reuse" \
    <"$scripts/prelum-r7-pin-stage.sh" >"$D/pin-uuid-stage.log" 2>&1
printf '%s\n' 12d7ac00-4bd5-4413-8da5-e6952965ffcf >"$D/printer-uuid"
check_served_key pin-uuid-held "$original_key"
remote "sh -s -- '$guest' '$marker' hold pin-uuid" <"$scripts/prelum-r7-identity.sh" >"$D/pin-uuid-held.log" 2>&1 || { cat "$D/pin-uuid-held.log" >&2; exit 1; }
cat "$D/pin-uuid-held.log"
check_pin_blocked uuid-held 2 "$D/pin-released-jobs.txt" 14

remote "sh -s -- '$guest' '$marker' retry pin-uuid" <"$scripts/prelum-r7-identity.sh" >"$D/pin-uuid-retry.log" 2>&1 || { cat "$D/pin-uuid-retry.log" >&2; exit 1; }
cat "$D/pin-uuid-retry.log"
check_pin_blocked uuid-retry 2 "$D/pin-released-jobs.txt" 14

mv "$D/printer-uuid" "$D/pin-held-printer-uuid.txt"
remote "sh -s -- '$guest' '$marker' release pin-uuid" <"$scripts/prelum-r7-identity.sh" >"$D/pin-uuid-release.log" 2>&1 || { cat "$D/pin-uuid-release.log" >&2; exit 1; }
cat "$D/pin-uuid-release.log"
remote "cat '$guest/fixture-arca/audit.log'" >"$D/pin-uuid-released-audit.log"
test "$(grep -c 'op=0x0011' "$D/pin-uuid-released-audit.log")" -eq 2
test "$(grep -cE 'prelum-ipp op=0x0011 .*status=0$' "$D/pin-uuid-released-audit.log")" -eq 2
remote "cat '$guest/pin-uuid-identity-released-jobs.log'" >"$D/pin-uuid-released-jobs.txt"
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" "$D/pin-uuid-released-jobs.txt" "$D/fixtures/quicklook.pdf" 15 >"$D/pin-uuid-released-pdf-checks.log"
cat "$D/pin-uuid-released-pdf-checks.log"


# Replay job 15's consumed handle with the original printer identity active.
check_served_key pin-replay "$original_key"
remote "sh -s -- '$guest' '$marker' replay pin-uuid" <"$scripts/prelum-r7-identity.sh" >"$D/pin-replay.log" 2>&1 || { cat "$D/pin-replay.log" >&2; exit 1; }
cat "$D/pin-replay.log"
remote "cat '$guest/fixture-arca/audit.log'" >"$D/pin-replay-audit.log"
test "$(grep -cE 'ui op=0x0068 .*status=0$' "$D/pin-replay-audit.log")" -eq 2
test "$(grep -c 'op=0x0011' "$D/pin-replay-audit.log")" -eq 3
test "$(grep -cE 'prelum-ipp op=0x0011 .*status=0$' "$D/pin-replay-audit.log")" -eq 2
grep -E 'op=0x0011' "$D/pin-replay-audit.log" | tail -1 | grep -E 'prelum-ipp op=0x0011 .*status=1$'
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" "$D/pin-uuid-released-jobs.txt" "$D/fixtures/quicklook.pdf" 15 >"$D/pin-replay-pdf-checks.log"
cat "$D/pin-replay-pdf-checks.log"


# Both PIN support and separate document submission are required.
remote "sh -s -- '$guest' '$marker' '$pin_spki' '$printer_uuid' reuse" \
    <"$scripts/prelum-r7-pin-stage.sh" >"$D/pin-fresh-stage.log" 2>&1
remote "cat '$guest/fixture-arca/audit.log'" >"$D/pin-capability-before-audit.log"
grep 'op=0x0011' "$D/pin-capability-before-audit.log" >"$D/pin-capability-before-redemptions.log"
check_missing_pin_capability() {
    capability=$1
    skipped_jobs=$2
    proof="$D/pin-no-$capability"
    touch "$D/no-$capability"
    remote "sh -s -- '$guest' '$marker' missing-capability 'pin-no-$capability'" \
        <"$scripts/prelum-r7-identity.sh" >"$proof.log" 2>&1 || { cat "$proof.log" >&2; exit 1; }
    cat "$proof.log"
    grep -F "fake printer: withholding $capability capability" "$D/simulator.log"
    remote "cat '$guest/fixture-arca/audit.log'" >"$proof-after-audit.log"
    grep 'op=0x0011' "$proof-after-audit.log" >"$proof-after-redemptions.log"
    cmp "$D/pin-capability-before-redemptions.log" "$proof-after-redemptions.log"
    remote "cat '$guest/pin-no-$capability-jobs.log'" >"$proof-jobs.txt"
    python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" \
        "$proof-jobs.txt" "$D/fixtures/quicklook.pdf" 15 "$skipped_jobs" >"$proof-pdf-checks.log"
    cat "$proof-pdf-checks.log"
    rm "$D/no-$capability"
}
check_missing_pin_capability send-document 16,17
check_missing_pin_capability job-password 16,17,18
check_missing_pin_capability create-job 16,17,18,19

# With all capabilities restored, the same handle must redeem once and print.
remote "sh -s -- '$guest' '$marker' fresh pin-fresh" <"$scripts/prelum-r7-identity.sh" >"$D/pin-fresh.log" 2>&1 || { cat "$D/pin-fresh.log" >&2; exit 1; }
cat "$D/pin-fresh.log"
remote "cat '$guest/fixture-arca/audit.log'" >"$D/pin-fresh-audit.log"
test "$(grep -cE 'ui op=0x0068 .*status=0$' "$D/pin-fresh-audit.log")" -eq 3
test "$(grep -c 'op=0x0011' "$D/pin-fresh-audit.log")" -eq 4
test "$(grep -cE 'prelum-ipp op=0x0011 .*status=0$' "$D/pin-fresh-audit.log")" -eq 3
grep -E 'op=0x0011' "$D/pin-fresh-audit.log" | tail -1 | grep -E 'prelum-ipp op=0x0011 .*status=0$'
remote "cat '$guest/pin-fresh-jobs.log'" >"$D/pin-fresh-jobs.txt"
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" "$D/pin-fresh-jobs.txt" "$D/fixtures/quicklook.pdf" 16 16,17,18,19 >"$D/pin-fresh-pdf-checks.log"
cat "$D/pin-fresh-pdf-checks.log"

# Check each response with and without a job ID, plus plain success without an ID.
desktop_job=20
printer_job=16
redemption_count=4
successful_redemptions=3
expected_responses="$D/pin-rejection-expected.txt"
: >"$expected_responses"
submit_pin_fixture() {
    proof="$D/$1"
    desktop_job=$((desktop_job + 1))
    redemption_count=$((redemption_count + 1))
    successful_redemptions=$((successful_redemptions + 1))
    remote "sh -s -- '$guest' '$marker' '$pin_spki' '$printer_uuid' reuse" \
        <"$scripts/prelum-r7-pin-stage.sh" >"$proof-stage.log" 2>&1
    remote "sh -s -- '$guest' '$marker' fresh '$2' '$desktop_job' '$3' '${4:-}' '${5:-}'" \
        <"$scripts/prelum-r7-identity.sh" >"$proof.log" 2>&1 || { cat "$proof.log" >&2; exit 1; }
    cat "$proof.log"
    remote "cat '$guest/fixture-arca/audit.log'" >"$proof-audit.log"
    test "$(grep -cE 'ui op=0x0068 .*status=0$' "$proof-audit.log")" -eq "$successful_redemptions"
    test "$(grep -c 'op=0x0011' "$proof-audit.log")" -eq "$redemption_count"
    test "$(grep -cE 'prelum-ipp op=0x0011 .*status=0$' "$proof-audit.log")" -eq "$successful_redemptions"
}
check_pin_rejection() {
    if [ "$2" = with-id ]; then
        printer_job=$((printer_job + 1))
        printf '%s %s %s\n' "$1" "$printer_job" "$3" >>"$expected_responses"
    else
        printf '%s - %s\n' "$1" "$3" >>"$expected_responses"
    fi
    submit_pin_fixture "pin-$1-$2-$3" pin-rejection "$1" "$2" "$3"
}
for response in ignored conflicting unsupported-group unsupported-status server-busy; do
    check_pin_rejection "$response" with-id canceled
    check_pin_rejection "$response" without-id none
done
check_pin_rejection success without-id none

# Pair each failed cancellation with a separate job and a freshly staged handle.
for cancel_result in ipp-refused http-refused dropped truncated; do
    check_pin_rejection ignored with-id "$cancel_result"
    printer_job=$((printer_job + 1))
    printf 'recovery %s %s\n' "$printer_job" "$cancel_result" >>"$expected_responses"
    submit_pin_fixture "pin-recovery-$cancel_result" pin-recovery "$cancel_result"
done
remote "cat '$guest/pin-recovery-truncated-jobs.log'" >"$D/pin-rejection-jobs.txt"
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" "$D/simulator.log" \
    "$D/pin-rejection-jobs.txt" "$D/fixtures/quicklook.pdf" 16 16,17,18,19 "$expected_responses" >"$D/pin-rejection-pdf-checks.log"
cat "$D/pin-rejection-pdf-checks.log"
