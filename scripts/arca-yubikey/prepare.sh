#!/bin/sh
set -eu
B=/var/tmp/arca-physical
G=/tmp/sshgate
S=/atrium/sockets/arca/arcad.sock
test -S /atrium/sockets/ssh-agent/control.sock
usbconfig list | grep -i yubikey
kldload -n hidraw
device=
for path in /dev/u2f/[0-9]*; do
    [ -c "$path" ] || continue
    [ -z "$device" ] || { echo "Attach only the test authenticator" >&2; exit 1; }
    device=$path
done
test -n "$device"
printf 'Dedicated YubiKey transport: %s\n' "$device"
install -d -m 0755 "$B/keys"
install -d -m 0755 -o sg-unlock "$B/ostiarius-state"
install -m 0644 "$B/45-atrium-ostiarius.toml" /etc/atrium/services.d/
install -m 0644 "$B/atrium-login" /etc/pam.d/atrium-login
printf '%s\n' correct-horse | pw usermod alice -h 0
daemon -f -o "$B/ostiarius.log" -u sg-unlock env OSTIARIUS_VERIFY_SOCK="$B/ostiarius-state/passcode.sock" \
    BROKER_SOCK=/atrium/sockets/portcullis/portcullis.sock OSTIARIUS_SIGILLUM_UID=0 OSTIARIUS_NO_BOOT=1 OSTIARIUS_SEAT="$B/ostiarius-state/seat" \
    OSTIARIUS_SOCK="$B/ostiarius-state/ostiarius.sock" "$B/ostiarius" --serve-prod
daemon -f -o "$B/sigillumd.log" env SIGILLUM_ARCA_UID=0 SIGILLUM_OSTIARIUS_UID=50093 \
    SIGILLUM_PORTCULLIS_UID=none SIGILLUM_DEV_UID=0 SIGILLUM_UI_UID=0 SIGILLUM_ARCA_SOCKET=none \
    SIGILLUM_SOCKET="$B/client.sock" SIGILLUM_DRIVER_SOCKET="$B/driver.sock" SIGILLUM_UI_SOCKET="$B/ui.sock" \
    SIGILLUM_KEY_DIR="$B/keys" SIGILLUM_OSTIARIUS_VERIFY="$B/ostiarius-state/passcode.sock" "$B/sigillumd"
i=0; while [ ! -S "$B/driver.sock" ] && [ "$i" -lt 15 ]; do sleep 1; i=$((i+1)); done
test -S "$B/driver.sock"
test -S "$B/ostiarius-state/passcode.sock"
daemon -f -o "$B/fido.log" "$B/sigillum-fido" --dev "$device" --connect "$B/driver.sock"
kill "$(cat "$G/arcad.pid")"
sleep 1
rm -f "$S"
daemon -f -p "$G/arcad.pid" -o "$B/arcad.log" env ARCA_DIR="$G/db" ARCA_SOCKET="$S" \
    ARCA_APP_REGISTRY=/var/run/atrium/app-registry ARCA_VESTIBULUM_UID=50093 ARCA_SERVICES=ssh-agent=65047 \
    ARCA_COURIER_REGISTRY="$G/none" ARCA_SHARE_REGISTRY="$G/none" ARCA_SIGILLUM_SOCKET="$B/client.sock" /var/tmp/sshgate-bin/arcad
i=0; while [ ! -S "$S" ] && [ "$i" -lt 15 ]; do sleep 1; i=$((i+1)); done
test -S "$S"
su -m sg-unlock -c "/var/tmp/sshgate-bin/arca-probe $S unlock alice correct-horse"
su -m alice -c 'arca-ssh new --verify-each-use physical-touch-fixture' > "$B/key-meta"
id=$(cut -d ' ' -f 1 "$B/key-meta")
test "${#id}" -eq 32
su -m alice -c "arca-ssh grant $id keys"
W=/home/alice/.atrium/workspaces/keys/work
cut -d ' ' -f 2- "$B/key-meta" > "$W/physical-key.pub"
chmod 0644 "$W/physical-key.pub"
echo 'Physical fixture staged'
