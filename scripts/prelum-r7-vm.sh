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
guest=/tmp/prelum-r7-desktop-gate
for path in "$base" "$code" "$vars" "$key"; do
    test -f "$path" || { echo "Missing gate input: $path" >&2; exit 2; }
done
for program in "$qemu" qemu-img ssh python3 openssl pdftotext pdfimages cargo clang; do
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
cleanup() {
    status=$?
    trap - EXIT
    if [ -n "$qemu_pid" ] && kill -0 "$qemu_pid" 2>/dev/null; then
        if [ -n "$guest_connected" ]; then
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
        else
            wait "$qemu_pid" || status=1
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
python3 -u "$scripts/prelum-fake-printer.py" --bind 127.0.0.1 --port "$ipps_port" \
    --state "$D/state" --spool "$D/spool" --tls-cert "$D/ipps.pem" --tls-key "$D/ipps.key" \
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
if ! remote "sh -s -- '$guest' '$ipps_port' '$marker'" <"$scripts/prelum-r7-guest.sh" >"$D/results.log" 2>&1; then
    remote "cd '$guest' && tail -60 app.log limend.log portcullis.log sheet.log mail.log" >"$D/guest-errors.log" 2>&1 || :
    cat "$D/results.log" >&2
    exit 1
fi
cat "$D/results.log"
python3 -I "$scripts/prelum-r7-pdf-check.py" "$D/spool" >"$D/pdf-checks.log"
cat "$D/pdf-checks.log"
