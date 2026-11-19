#!/bin/sh
# Runs in the dedicated guest over a connection without a terminal.
set -eu
B=/var/tmp/arca-physical
W=/home/alice/.atrium/workspaces/keys/work
trial=$(mktemp -d "$B/run.XXXXXXXX")
tag=${trial##*/}
echo "Evidence: $trial"
log=$B/sigillumd.log
first=$1
tries=0
until tail -n "+$first" "$log" | grep -Fq 'trusted UI connected'; do
    tries=$((tries + 1))
    [ "$tries" -lt 30 ] || { echo 'PIN reader did not connect' >&2; exit 1; }
    sleep 1
done
if [ ! -s "$B/keys/alice.json" ]; then
    echo 'Enroll the test key: enter its PIN and touch when prompted.'
    timeout 150 su -m sg-unlock -c "$B/sigillum-probe enrol alice YubiKey $B/client.sock" > "$trial/enrollment"
    grep -F 'enrol: KeyEnrolled' "$trial/enrollment"
fi
printf 'alice %s\n' "$(cut -d ' ' -f 2- "$B/key-meta")" > "$trial/allowed"
sign='set -eu
    msg=$1
    test ! -e "$msg" && test ! -e "$msg.sig" || exit 1
    echo "$msg" > "$msg"
    LC_ALL=C exec ssh-keygen -Y sign -n file -f physical-key.pub "$msg"'
export sign
for n in 1 2 3; do
    msg=physical-$tag-$n
    first=$(( $(wc -l < "$log") + 1 ))
    if [ "$n" -eq 3 ]; then
        echo 'Cancel signing 3: press Return at the empty PIN prompt.'
        decision='cancelled on the sheet'
    else
        echo "Signing $n: enter the PIN, then wait for the new touch prompt before touching."
        decision='verified by security key'
    fi
    export msg
    su -m alice -c 'cd /tmp && exec portcullis workspace exec keys -- timeout 150 /bin/sh -c "$sign" sh "$msg"' > "$trial/$n.out"
    cat "$trial/$n.out"
    test "$(cat "$W/$msg")" = "$msg"
    tail -n "+$first" "$log" > "$trial/$n.audit"
    sh /root/arca-yubikey-check-result.sh "$trial/$n.audit" "$trial/$n.out" "$W/$msg" "$trial/allowed" "$decision"
done
echo 'PASS: two touch-approved SSH signatures verified; cancellation refused and produced no signature'
